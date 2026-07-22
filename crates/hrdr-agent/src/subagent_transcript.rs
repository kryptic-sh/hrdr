//! Per-sub-agent transcript: an append-only JSONL log of one delegated `task`
//! run, written independently of the parent session so a sub-agent that dies
//! mid-run leaves all completed work recoverable on disk.
//!
//! Each line is a [`Record`] — a complete, serializable projection of the
//! sub-agent's `AgentEvent` stream: tool calls keep their full args and results,
//! so the on-disk record shows exactly which files and paths a tool touched. On
//! read, each `Record` maps back to an `AgentEvent` and folds through the SAME
//! [`crate::apply_event`] reducer as the main transcript, so a sub-agent's
//! durable record renders identically to the main agent's.
//!
//! Persistence is best-effort: every write error is swallowed, because writing
//! a transcript must never break the actual sub-agent run. A brand-new,
//! never-saved session has no id yet, so the very first sub-agent spawned
//! before the first autosave is not persisted (the dir cell is still empty).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// How a sub-agent was spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnKind {
    Blocking,
    Background,
}

/// Terminal status of a sub-agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndStatus {
    Ok,
    Failed,
    Panicked,
    Cancelled,
}

/// One line in a sub-agent transcript. A complete, serializable projection of
/// the sub-agent's `AgentEvent` stream — tool calls keep their full args and
/// results — plus the `Start`/`End`/`Error` framing needed for orphan
/// detection. Serialized with a `t` discriminator so a reader can dispatch on
/// the record kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Start {
        model: String,
        label: String,
        kind: SpawnKind,
        prompt: String,
    },
    Reasoning {
        text: String,
    },
    Text {
        chunk: String,
    },
    ToolStart {
        id: String,
        name: String,
        args: String,
    },
    ToolOutput {
        id: String,
        chunk: String,
    },
    ToolEnd {
        id: String,
        name: String,
        result: String,
        ok: bool,
    },
    Notice {
        msg: String,
    },
    Steered {
        text: String,
    },
    Error {
        msg: String,
    },
    End {
        status: EndStatus,
        /// Byte length of the sub-agent's trimmed text output at the terminal
        /// point — the same measure on the blocking and background paths, so runs
        /// are comparable. `0` on `Panicked`/`Cancelled`, where the output was
        /// never collected. A size hint only; it gates nothing.
        bytes: usize,
    },
}

impl Record {
    /// Project a live agent event onto the transcript record to persist, if any.
    /// The write side of the sub-agent transcript: keeps tool args and results
    /// intact. Bulky bookkeeping (`Usage`, `History`) and non-transcript signals
    /// (`TurnDone`, `TodoUpdated`) are dropped.
    pub fn from_event(ev: &crate::AgentEvent) -> Option<Record> {
        use crate::AgentEvent;
        match ev {
            AgentEvent::Reasoning(t) => Some(Record::Reasoning { text: t.clone() }),
            AgentEvent::Text(t) => Some(Record::Text { chunk: t.clone() }),
            AgentEvent::ToolStart { id, name, args } => Some(Record::ToolStart {
                id: id.clone(),
                name: name.clone(),
                args: args.clone(),
            }),
            AgentEvent::ToolOutput { id, chunk } => Some(Record::ToolOutput {
                id: id.clone(),
                chunk: chunk.clone(),
            }),
            AgentEvent::ToolEnd {
                id,
                name,
                result,
                ok,
            } => Some(Record::ToolEnd {
                id: id.clone(),
                name: name.clone(),
                result: result.clone(),
                ok: *ok,
            }),
            AgentEvent::Notice(n) => Some(Record::Notice { msg: n.clone() }),
            AgentEvent::Steered(s) => Some(Record::Steered { text: s.clone() }),
            AgentEvent::Usage { .. }
            | AgentEvent::History(_)
            | AgentEvent::TodoUpdated(_)
            | AgentEvent::TurnDone => None,
        }
    }

    /// Map this record back to the `AgentEvent` the shared reducer expects, if
    /// it carries transcript content. The read side of the sub-agent transcript.
    ///
    /// `Start` opens the folded transcript with the task as a user turn
    /// (`Steered`), matching the live path (`delegation.rs` records the prompt as
    /// a `Steered` event before the run). `Error` surfaces as a `Notice`. `End`
    /// is pure framing and folds to nothing.
    // Used by `read_transcript` (and its test); the crash-recovery UI (a later
    // WISHLIST item) is its non-test consumer.
    #[allow(dead_code)]
    pub fn as_event(&self) -> Option<crate::AgentEvent> {
        use crate::AgentEvent;
        match self {
            Record::Start { prompt, .. } => Some(AgentEvent::Steered(prompt.clone())),
            Record::Reasoning { text } => Some(AgentEvent::Reasoning(text.clone())),
            Record::Text { chunk } => Some(AgentEvent::Text(chunk.clone())),
            Record::ToolStart { id, name, args } => Some(AgentEvent::ToolStart {
                id: id.clone(),
                name: name.clone(),
                args: args.clone(),
            }),
            Record::ToolOutput { id, chunk } => Some(AgentEvent::ToolOutput {
                id: id.clone(),
                chunk: chunk.clone(),
            }),
            Record::ToolEnd {
                id,
                name,
                result,
                ok,
            } => Some(AgentEvent::ToolEnd {
                id: id.clone(),
                name: name.clone(),
                result: result.clone(),
                ok: *ok,
            }),
            Record::Notice { msg } => Some(AgentEvent::Notice(msg.clone())),
            Record::Steered { text } => Some(AgentEvent::Steered(text.clone())),
            Record::Error { msg } => Some(AgentEvent::Notice(msg.clone())),
            Record::End { .. } => None,
        }
    }
}

/// An open append-only transcript file for one sub-agent run.
pub struct SubagentTranscript {
    file: File,
    path: std::path::PathBuf,
}

impl SubagentTranscript {
    /// The transcript file's path, so a caller can point a reader at it later.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SubagentTranscript {
    /// Create `dir/<id>.jsonl` for one run, creating `dir` if needed.
    ///
    /// **Exclusive.** A run owns its file outright: if `id` is already taken this
    /// returns [`std::io::ErrorKind::AlreadyExists`] so the caller can pick the
    /// next id (see `open_next` in `lib.rs`). Opening in plain append mode would
    /// be wrong — the transcript dir is keyed by *session id* and so survives a
    /// resume, while the id counter restarts at 0 in each process, so a resumed
    /// session would append a fresh run onto a previous run's file. That yields a
    /// file with two `Start`s and two `End`s, and makes [`is_complete`] report a
    /// genuinely orphaned run as complete — defeating the whole point of the log.
    pub fn create(dir: &Path, id: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // The transcript holds the sub-agent's full prompt and output. Keep the
        // directory owner-only; the file inherits protection from it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let mut opts = OpenOptions::new();
        opts.create_new(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let path = dir.join(format!("{id}.jsonl"));
        let file = opts.open(&path)?;
        Ok(Self { file, path })
    }

    /// Append one record as a JSON line and flush. All errors are swallowed: a
    /// failed transcript write must never break the sub-agent run.
    pub fn write(&mut self, rec: &Record) {
        if let Ok(mut line) = serde_json::to_string(rec) {
            line.push('\n');
            let _ = self.file.write_all(line.as_bytes());
            let _ = self.file.flush();
        }
    }
}

/// Whether a transcript file ends in an `End` record. A file with no `End` line
/// is an orphan: the sub-agent crashed or is still running.
// Used by tests now; the crash-recovery UI (a later WISHLIST item) is its
// non-test consumer.
#[allow(dead_code)]
pub fn is_complete(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut last = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            last = Some(line);
        }
    }
    match last {
        Some(l) => matches!(serde_json::from_str::<Record>(&l), Ok(Record::End { .. })),
        None => false,
    }
}

/// Read a sub-agent transcript file and fold it into a Vec<Entry> using the
/// SAME reducer as the main transcript, so the on-disk sub-agent record renders
/// identically (tool args + results intact). Best-effort: unparsable lines are skipped.
// Used by tests now; the crash-recovery UI (a later WISHLIST item) is its
// non-test consumer.
#[allow(dead_code)]
pub fn read_transcript(path: &Path) -> Vec<crate::Entry> {
    let mut entries = Vec::new();
    let Ok(file) = File::open(path) else {
        return entries;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Record>(&line) else {
            continue;
        };
        // Each record folds through the shared event reducer.
        if let Some(ev) = rec.as_event() {
            crate::apply_event(&mut entries, &ev);
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serializes_with_t_tag_and_snake_case() {
        let start = Record::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Background,
            prompt: "p".into(),
        };
        let s = serde_json::to_string(&start).unwrap();
        assert!(s.contains(r#""t":"start""#), "got {s}");
        assert!(s.contains(r#""kind":"background""#), "got {s}");
        // Round-trips.
        assert_eq!(serde_json::from_str::<Record>(&s).unwrap(), start);

        // A tool call keeps its full args on the wire (the whole point of the
        // complete projection).
        let tool = Record::ToolStart {
            id: "t1".into(),
            name: "edit".into(),
            args: r#"{"path":"src/main.rs"}"#.into(),
        };
        let s = serde_json::to_string(&tool).unwrap();
        assert!(s.contains(r#""t":"tool_start""#), "got {s}");
        assert!(s.contains("src/main.rs"), "args survive serialization: {s}");
        assert_eq!(serde_json::from_str::<Record>(&s).unwrap(), tool);

        let end = Record::End {
            status: EndStatus::Panicked,
            bytes: 3,
        };
        let s = serde_json::to_string(&end).unwrap();
        assert!(
            s.contains(r#""t":"end""#) && s.contains(r#""status":"panicked""#),
            "got {s}"
        );
    }

    #[test]
    fn write_appends_one_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = SubagentTranscript::create(dir.path(), "001-x").unwrap();
        t.write(&Record::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Blocking,
            prompt: "p".into(),
        });
        t.write(&Record::Text {
            chunk: "hello".into(),
        });
        t.write(&Record::End {
            status: EndStatus::Ok,
            bytes: 5,
        });
        let body = std::fs::read_to_string(dir.path().join("001-x.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3, "one line per record: {body:?}");
        for l in &lines {
            serde_json::from_str::<Record>(l).expect("each line is a standalone Record");
        }
    }

    /// The whole point of the complete projection: a tool call's args (the path
    /// it touched) and its result (the diff it produced) survive to disk and
    /// back, folded into an `EntryKind::Tool` by the SAME reducer as the main
    /// transcript.
    #[test]
    fn read_transcript_preserves_tool_args_and_result() {
        use crate::EntryKind;
        let dir = tempfile::tempdir().unwrap();
        let mut t = SubagentTranscript::create(dir.path(), "003-edit").unwrap();
        let args = r#"{"path":"src/lib.rs"}"#;
        let result = "@@ -1 +1 @@\n-old line\n+new line";
        t.write(&Record::Start {
            model: "m".into(),
            label: "edit-task".into(),
            kind: SpawnKind::Blocking,
            prompt: "edit the file".into(),
        });
        t.write(&Record::ToolStart {
            id: "call-1".into(),
            name: "edit".into(),
            args: args.into(),
        });
        t.write(&Record::ToolEnd {
            id: "call-1".into(),
            name: "edit".into(),
            result: result.into(),
            ok: true,
        });
        t.write(&Record::End {
            status: EndStatus::Ok,
            bytes: 0,
        });

        let entries = read_transcript(&dir.path().join("003-edit.jsonl"));
        let tool = entries
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::Tool {
                    name,
                    args,
                    result,
                    ok,
                    ..
                } => Some((name.clone(), args.clone(), result.clone(), *ok)),
                _ => None,
            })
            .expect("a folded Tool entry");
        assert_eq!(tool.0, "edit");
        assert!(
            tool.1.contains("src/lib.rs"),
            "args (path) survive: {}",
            tool.1
        );
        assert!(
            tool.2.contains("new line"),
            "result (diff) survives: {}",
            tool.2
        );
        assert!(tool.3, "ok flag survives");
    }

    /// A run owns its file. The dir is keyed by session id and survives a resume,
    /// but the id counter restarts at 0 each process — so without exclusive
    /// creation a resumed session's first task would append onto the previous
    /// run's log, producing a file with two `Start`s and making an orphaned run
    /// look complete.
    #[test]
    fn create_refuses_to_reuse_an_existing_run_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = SubagentTranscript::create(dir.path(), "000-sub-task").unwrap();
        first.write(&Record::Start {
            model: "m".into(),
            label: "sub-task".into(),
            kind: SpawnKind::Blocking,
            prompt: "first run".into(),
        });
        // No End: the first run crashed. It must stay an identifiable orphan.
        drop(first);

        let err = match SubagentTranscript::create(dir.path(), "000-sub-task") {
            Ok(_) => panic!("an id already on disk must not be reopened"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let path = dir.path().join("000-sub-task.jsonl");
        assert!(!is_complete(&path), "the crashed run is still an orphan");
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1, "untouched by the second attempt");
    }

    #[cfg(unix)]
    #[test]
    fn transcript_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("subagents");
        let _t = SubagentTranscript::create(&root, "000-x").unwrap();
        let file_mode = std::fs::metadata(root.join("000-x.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode();
        // The transcript carries the sub-agent's full prompt and output.
        assert_eq!(file_mode & 0o777, 0o600, "transcript must be 0600");
        assert_eq!(dir_mode & 0o777, 0o700, "transcript dir must be 0700");
    }

    #[test]
    fn is_complete_flags_orphan_and_preserves_partial_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("002-x.jsonl");
        // One run holds one handle for its whole life — the file is never
        // reopened, so this mirrors the real spawn paths.
        let mut t = SubagentTranscript::create(dir.path(), "002-x").unwrap();
        t.write(&Record::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Blocking,
            prompt: "p".into(),
        });
        t.write(&Record::Text {
            chunk: "done work".into(),
        });

        // Mid-run, before any End: an orphan whose completed work is on disk.
        assert!(!is_complete(&path), "no End line => orphan");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("done work"),
            "partial work survives the crash"
        );

        // The terminal event lands on the same handle the run has held all along.
        t.write(&Record::End {
            status: EndStatus::Failed,
            bytes: 9,
        });
        assert!(is_complete(&path), "End line => complete");
    }

    /// A sub-agent transcript is a pure fold of the `AgentEvent` stream. This
    /// round-trips an event sequence (Steered + Text + ToolStart/ToolEnd) through
    /// disk and back, asserting the folded transcript reconstructs in log order
    /// with tool args and results intact.
    #[test]
    fn read_transcript_folds_an_event_stream_in_order() {
        use crate::{AgentEvent, EntryKind};
        let dir = tempfile::tempdir().unwrap();
        let mut t = SubagentTranscript::create(dir.path(), "004-main").unwrap();

        // Written order: user turn, assistant reply, a tool call (args + result).
        t.write(&Record::from_event(&AgentEvent::Steered("audit the config".into())).unwrap());
        t.write(&Record::from_event(&AgentEvent::Text("looking now".into())).unwrap());
        t.write(
            &Record::from_event(&AgentEvent::ToolStart {
                id: "call-1".into(),
                name: "read".into(),
                args: r#"{"path":"config.toml"}"#.into(),
            })
            .unwrap(),
        );
        t.write(
            &Record::from_event(&AgentEvent::ToolEnd {
                id: "call-1".into(),
                name: "read".into(),
                result: "port = 8080".into(),
                ok: true,
            })
            .unwrap(),
        );

        let entries = read_transcript(&dir.path().join("004-main.jsonl"));

        // The event-derived tool entry carries its args AND result.
        let tool = entries
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::Tool { args, result, .. } => Some((args.clone(), result.clone())),
                _ => None,
            })
            .expect("folded Tool entry present");
        assert!(
            tool.0.contains("config.toml"),
            "tool args survive: {}",
            tool.0
        );
        assert!(tool.1.contains("8080"), "tool result survives: {}", tool.1);

        // The assistant text folded from the event stream is present.
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::Assistant(s) if s == "looking now")),
            "assistant text present"
        );

        // Overall ordering matches the written sequence.
        let kinds: Vec<&EntryKind> = entries.iter().map(|e| &e.kind).collect();
        assert!(
            matches!(kinds.as_slice(),
                [
                    EntryKind::User(u),
                    EntryKind::Assistant(_),
                    EntryKind::Tool { .. },
                ] if u == "audit the config"
            ),
            "reconstructed in log order: {kinds:?}"
        );
    }

    #[test]
    fn is_complete_is_false_for_missing_file() {
        assert!(!is_complete(Path::new("/nonexistent/does/not/exist.jsonl")));
    }

    #[test]
    fn open_error_is_returned_not_panicked() {
        // A path whose parent cannot be created (a file where a dir is needed).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let bad_dir = blocker.join("subdir"); // parent is a file
        assert!(SubagentTranscript::create(&bad_dir, "id").is_err());
    }
}

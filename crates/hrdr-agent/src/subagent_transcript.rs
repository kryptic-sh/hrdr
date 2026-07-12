//! Per-sub-agent transcript: an append-only JSONL log of one delegated `task`
//! run, written independently of the parent session so a sub-agent that dies
//! mid-run leaves all completed work recoverable on disk.
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

/// One line in a sub-agent transcript. Serialized with a `t` discriminator so a
/// reader can dispatch on the event kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event {
    Start {
        model: String,
        label: String,
        kind: SpawnKind,
        prompt: String,
    },
    Text {
        chunk: String,
    },
    Tool {
        name: String,
    },
    Error {
        msg: String,
    },
    End {
        status: EndStatus,
        /// Approximate size hint: the byte length of the accumulated output at
        /// the terminal point. Trimming and availability vary by path (untrimmed
        /// on the blocking path, trimmed on background Ok, `0` on panic/cancel),
        /// so treat it as a rough indicator, not an exact count. Not used to gate
        /// anything.
        bytes: usize,
    },
}

/// An open append-only transcript file for one sub-agent run.
pub struct SubagentTranscript {
    file: File,
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
        let file = opts.open(dir.join(format!("{id}.jsonl")))?;
        Ok(Self { file })
    }

    /// Append one event as a JSON line and flush. All errors are swallowed: a
    /// failed transcript write must never break the sub-agent run.
    pub fn write(&mut self, ev: &Event) {
        if let Ok(mut line) = serde_json::to_string(ev) {
            line.push('\n');
            let _ = self.file.write_all(line.as_bytes());
            let _ = self.file.flush();
        }
    }
}

/// Whether a transcript file ends in an `End` event. A file with no `End` line
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
        Some(l) => matches!(serde_json::from_str::<Event>(&l), Ok(Event::End { .. })),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_with_t_tag_and_snake_case() {
        let start = Event::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Background,
            prompt: "p".into(),
        };
        let s = serde_json::to_string(&start).unwrap();
        assert!(s.contains(r#""t":"start""#), "got {s}");
        assert!(s.contains(r#""kind":"background""#), "got {s}");
        // Round-trips.
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), start);

        let end = Event::End {
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
    fn write_appends_one_line_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = SubagentTranscript::create(dir.path(), "001-x").unwrap();
        t.write(&Event::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Blocking,
            prompt: "p".into(),
        });
        t.write(&Event::Text {
            chunk: "hello".into(),
        });
        t.write(&Event::End {
            status: EndStatus::Ok,
            bytes: 5,
        });
        let body = std::fs::read_to_string(dir.path().join("001-x.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3, "one line per event: {body:?}");
        for l in &lines {
            serde_json::from_str::<Event>(l).expect("each line is a standalone Event");
        }
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
        first.write(&Event::Start {
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
        t.write(&Event::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Blocking,
            prompt: "p".into(),
        });
        t.write(&Event::Text {
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
        t.write(&Event::End {
            status: EndStatus::Failed,
            bytes: 9,
        });
        assert!(is_complete(&path), "End line => complete");
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

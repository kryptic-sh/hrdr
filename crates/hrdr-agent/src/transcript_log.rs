//! An agent's durable transcript: an append-only JSONL log of one agent's run,
//! written as the run happens so an agent that dies mid-turn leaves all
//! completed work recoverable on disk.
//!
//! **One log per agent, one format for all of them.** The session's own agent
//! attaches to its `<session-id>.jsonl` ([`AgentRegistry::attach_transcript`]);
//! a delegated agent owns a `<NNN-label>.jsonl` of its own. Neither is a
//! different kind of artifact, and neither has a reader of its own: each line is
//! a [`Record`], a complete serializable projection of that agent's `AgentEvent`
//! stream — tool calls keep their full args and results, so the record shows
//! exactly which files and paths a tool touched — and on read each `Record` maps
//! back to an `AgentEvent` and folds through the SAME [`crate::apply_event`]
//! reducer the live panes use. So a durable transcript renders identically to
//! the run it recorded, whichever agent produced it.
//!
//! Persistence is best-effort: every write error is swallowed, because writing a
//! transcript must never break the run it is recording. A brand-new, never-saved
//! session has no id yet, so an agent spawned before the first autosave is not
//! persisted (the dir cell is still empty).
//!
//! [`AgentRegistry::attach_transcript`]: crate::AgentRegistry::attach_transcript
//!
//! Best-effort is *not* licence to leave a half-written line behind, though: a
//! torn line costs two records (the fragment, and whatever is appended onto it)
//! and breaks every line-by-line reader of the file. So the two sides hold a
//! line-atomicity contract — [`TranscriptLog::write`] either lands a whole
//! record or rolls its bytes back, and the readers here skip a damaged line
//! instead of stopping at it (a torn file from an older build must still resume).
//!
//! Streamed deltas are **coalesced** before they reach the disk: one `write(2)`
//! per token is pure waste — a few bytes of payload behind ~25 bytes of JSON
//! framing — when consecutive deltas of one stream fold to exactly the same
//! transcript as a single record holding their concatenation. See
//! [`TranscriptLog::write`] for the buffer and the boundaries that end it.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Terminal status of an agent's run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndStatus {
    Ok,
    Failed,
    Panicked,
    Cancelled,
}

/// One line in an agent's transcript. A complete, serializable projection of
/// that agent's `AgentEvent` stream — tool calls keep their full args and
/// results — plus the `Start`/`End`/`Error` framing needed for orphan
/// detection. Serialized with a `t` discriminator so a reader can dispatch on
/// the record kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Start {
        model: String,
        label: String,
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

/// How much coalesced delta payload buys a line of its own. A streamed delta is
/// a few bytes behind ~25 bytes of JSON framing plus a `write(2)`, so writing one
/// per token spends most of the file and nearly all of the syscalls on framing.
/// At this size the framing is noise and a crash can lose at most this much
/// streamed prose — which [`COALESCE_AGE`] bounds in wall-clock terms too.
const COALESCE_BYTES: usize = 512;

/// How long a partial line may sit unwritten. The size threshold alone is a
/// *token* bound, and a slow model (or a long, quiet reasoning burst) can hold a
/// buffer open for minutes under it — so the crash trail would lag arbitrarily
/// far behind the run in wall-clock time. This caps that lag: a run being
/// watched, or killed, is at most this stale on disk.
const COALESCE_AGE: Duration = Duration::from_millis(500);

/// An open append-only transcript file for one sub-agent run.
pub struct TranscriptLog {
    file: File,
    path: std::path::PathBuf,
    /// The file's byte length as this handle has written it — the rollback
    /// point for a torn append. Tracked instead of `fstat`ed per record: only
    /// this handle appends, so the length is the open length plus every
    /// successful line, and a partial write rolls back to the last boundary.
    len: u64,
    /// The file ends **mid-record**: a previous append got a partial write that
    /// could not be rolled back, or the file we opened already ended without a
    /// newline (a torn tail left by an earlier process). The next record is
    /// prefixed with `\n` so the fragment stays one broken line instead of
    /// swallowing the next record too. See [`Self::write`].
    torn: bool,
    /// The streamed record still accumulating deltas, not yet on disk. Held as a
    /// `Record` rather than a loose string so the flush cannot mislabel it.
    pending: Option<Record>,
    /// When [`Self::pending`] took its first delta, for the [`COALESCE_AGE`] cap.
    opened_at: Option<Instant>,
}

/// Append `next` onto the open streamed record `open`, if they are the same
/// stream. Only the three delta kinds coalesce, and only onto their own kind —
/// [`crate::apply_event`] folds each by pushing onto the entry it already has
/// open, so N consecutive deltas and one record holding their concatenation
/// reconstruct the identical transcript. `ToolOutput` additionally has to match
/// on `id`: two tools' output interleaved into one record would fold onto the
/// wrong call.
///
/// Everything else returns `false` and is therefore a boundary — which covers
/// the ones that matter on their own: reasoning giving way to output (different
/// kinds), and a tool call opening or closing mid-stream.
fn merge(open: &mut Record, next: &Record) -> bool {
    match (open, next) {
        (Record::Reasoning { text }, Record::Reasoning { text: more }) => {
            text.push_str(more);
            true
        }
        (Record::Text { chunk }, Record::Text { chunk: more }) => {
            chunk.push_str(more);
            true
        }
        (
            Record::ToolOutput { id, chunk },
            Record::ToolOutput {
                id: next_id,
                chunk: more,
            },
        ) if id == next_id => {
            chunk.push_str(more);
            true
        }
        _ => false,
    }
}

/// The coalesced payload a streamed record carries, for the size threshold.
/// `None` for a record that is not a delta and so is never buffered.
fn delta_len(rec: &Record) -> Option<usize> {
    match rec {
        Record::Reasoning { text } => Some(text.len()),
        Record::Text { chunk } | Record::ToolOutput { chunk, .. } => Some(chunk.len()),
        _ => None,
    }
}

impl TranscriptLog {
    /// The transcript file's path, so a caller can point a reader at it later.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TranscriptLog {
    /// Create `dir/<id>.jsonl` for one run, creating `dir` if needed.
    ///
    /// **Exclusive.** A run owns its file outright: if `id` is already taken this
    /// returns [`std::io::ErrorKind::AlreadyExists`] so the caller can pick the
    /// next id (see `open_next` in `lib.rs`). Opening in plain append mode would
    /// be wrong — the transcript dir is keyed by *session id* and so survives a
    /// resume, while the id counter restarts at 0 in each process, so a resumed
    /// session would append a fresh run onto a previous run's file. That yields a
    /// file with two `Start`s and two `End`s, and makes an orphan check report a
    /// genuinely orphaned run as complete — defeating the whole point of the log.
    pub fn create(dir: &Path, id: &str) -> std::io::Result<Self> {
        // The transcript holds the sub-agent's full prompt and output. Keep the
        // directory owner-only; the file inherits protection from it.
        crate::auth::create_dir_owner_only(dir)?;
        // Owner-only too, not just the directory: a transcript is a verbatim
        // prompt/output log ([`hrdr_llm::owner_only_options`] documents what that
        // buys on each platform).
        let mut opts = hrdr_llm::owner_only_options();
        opts.create_new(true).append(true);
        let path = dir.join(format!("{id}.jsonl"));
        let file = opts.open(&path)?;
        Ok(Self {
            file,
            path,
            // Exclusively created: the file is empty, so it starts on a boundary.
            len: 0,
            torn: false,
            pending: None,
            opened_at: None,
        })
    }

    /// Open `path` for appending, creating it (and its parent dirs) if absent.
    ///
    /// **Non-exclusive**, unlike [`create`](Self::create): the main agent's
    /// transcript has a *stable* id across resumes — the file survives with the
    /// session and the process reattaches to it — so a resumed session must
    /// continue appending to its existing jsonl, not refuse it. (A sub-agent's id
    /// counter restarts each process, which is exactly why its runs own their
    /// files outright.) The existing records stay put; new events land after them,
    /// and a later [`read_transcript`] folds old and new together in order.
    pub fn append(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            // The transcript holds the agent's full prompt and output; keep its
            // directory owner-only (the file inherits from the mode below).
            crate::auth::create_dir_owner_only(parent)?;
        }
        // Same owner-only policy as [`create`](Self::create) — see
        // [`hrdr_llm::owner_only_options`].
        let mut opts = hrdr_llm::owner_only_options();
        opts.create(true).append(true);
        let file = opts.open(path)?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            file,
            path: path.to_path_buf(),
            len,
            // A file that does not end in a newline was cut mid-record (a crash
            // or a full disk during an earlier append). Resuming onto it must not
            // glue this session's first record onto that fragment.
            torn: ends_mid_line(path),
            pending: None,
            opened_at: None,
        })
    }

    /// Record one event. A streamed delta joins the open line instead of costing
    /// a `write(2)` of its own; everything else lands immediately, after whatever
    /// was buffered, so the file order is always the call order.
    ///
    /// The buffer ends — and the line lands — at whichever comes first:
    ///
    /// * **A boundary.** Any record [`merge`] rejects: a different delta kind
    ///   (reasoning giving way to output), another tool's output, or a
    ///   non-streaming record at all (`ToolStart`/`ToolEnd`/`Notice`/`End`, …).
    ///   These are exactly the points a reader distinguishes, so no coalescing
    ///   can ever blur one.
    /// * **[`COALESCE_BYTES`] of payload**, so a long stream still lands in
    ///   pieces rather than as one enormous line at the end.
    /// * **[`COALESCE_AGE`]**, so a slow stream's trail cannot lag the run by an
    ///   unbounded amount of wall-clock time.
    /// * **[`Self::flush`]**, which the owner calls at a turn boundary, and
    ///   `Drop` calls for a writer that is abandoned mid-stream.
    ///
    /// A crash inside the window loses the buffered deltas — bounded by the two
    /// thresholds above, and only ever streamed prose: a tool's `args` and its
    /// `result` ride on non-streaming records, and `ToolEnd` restates the whole
    /// result that the `ToolOutput` deltas were progressively building.
    pub fn write(&mut self, rec: &Record) {
        // Same stream as the open line: append and stay buffered, unless this
        // delta is what tips it over a threshold.
        let full = match self.pending.as_mut() {
            Some(open) => {
                merge(open, rec).then(|| delta_len(open).is_some_and(|n| n >= COALESCE_BYTES))
            }
            None => None,
        };
        if let Some(full) = full {
            if full || self.opened_at.is_some_and(|t| t.elapsed() >= COALESCE_AGE) {
                self.flush();
            }
            return;
        }
        // A boundary: the buffered line goes out first, so `rec` cannot overtake
        // the deltas that preceded it.
        self.flush();
        // A delta that is already worth a line of its own opens no buffer: it
        // amortizes its own framing, and buffering it would only delay it (and
        // copy it) waiting for a partner it does not need.
        if delta_len(rec).is_some_and(|n| n < COALESCE_BYTES) {
            self.pending = Some(rec.clone());
            self.opened_at = Some(Instant::now());
            return;
        }
        self.append_line(rec);
    }

    /// Land the buffered deltas, if any. Idempotent, and the boundary an owner
    /// asserts by hand: the end of a turn, where the next event may be minutes
    /// away and the on-disk transcript is what a resume folds back.
    pub fn flush(&mut self) {
        self.opened_at = None;
        if let Some(rec) = self.pending.take() {
            self.append_line(&rec);
        }
    }

    /// Append one record as **one whole line** and flush. All errors are
    /// swallowed: a failed transcript write must never break the agent's run.
    ///
    /// A torn line loses two records, not one — the fragment and whatever gets
    /// appended after it — and makes the whole line unparsable, so the guarantee
    /// here is that a record either lands in full or leaves no trace:
    ///
    /// * The record is serialized into a single buffer *including* its trailing
    ///   newline and written with one [`append_all`] loop — never a `write!` per
    ///   field, never a flush that could split it.
    /// * A partial write (the real case seen in the wild: `ENOSPC` with 21 bytes
    ///   of a `Reasoning` record on disk, the next record appended straight onto
    ///   the fragment) is **rolled back** to the pre-write length, so the file
    ///   still ends on a record boundary. Only this handle appends to the file,
    ///   under the `Mutex` its owner holds, so the truncated tail can only ever
    ///   be our own partial bytes.
    /// * If even the rollback fails, [`Self::torn`] is set and the next record
    ///   opens with a newline — the damage stays confined to one line, which
    ///   [`read_transcript`] skips.
    fn append_line(&mut self, rec: &Record) {
        let Ok(json) = serde_json::to_string(rec) else {
            return;
        };
        let mut line = String::with_capacity(json.len() + 2);
        if self.torn {
            line.push('\n');
        }
        line.push_str(&json);
        line.push('\n');
        // The record boundary to roll back to if this append tears.
        let before = self.len;
        match append_all(&mut self.file, line.as_bytes()) {
            Ok(()) => {
                self.torn = false;
                self.len += line.len() as u64;
            }
            // Nothing reached the disk: the file is untouched, so it is exactly as
            // torn (or not) as it was before.
            Err(0) => {}
            Err(_partial) => {
                let rolled = self.file.set_len(before).is_ok();
                if !rolled {
                    self.torn = true;
                }
            }
        }
        let _ = self.file.flush();
    }
}

/// A writer that goes away mid-stream still owes its buffered deltas to the
/// file: a sub-agent whose run ends, a session switch that detaches the main
/// agent's writer, a cancelled task whose registry entry is pruned. Without this
/// the last partial line of every run would be dropped on the floor.
impl Drop for TranscriptLog {
    fn drop(&mut self) {
        self.flush();
    }
}

/// [`Write::write_all`], but reporting how many bytes made it out when it fails.
/// `write_all` collapses that to a bare error, and rolling a partial record back
/// needs to know whether anything landed at all.
fn append_all(w: &mut impl Write, buf: &[u8]) -> Result<(), usize> {
    let mut done = 0;
    while done < buf.len() {
        match w.write(&buf[done..]) {
            // A zero-length write makes no progress; treat it as a stalled write
            // rather than spinning forever.
            Ok(0) => return Err(done),
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(done),
        }
    }
    Ok(())
}

/// Whether `path` ends mid-record — non-empty and not terminated by a newline.
/// Read through its own handle: the transcript's own handle is append/write-only.
/// Unreadable or absent counts as "not torn": a fresh file starts on a boundary.
fn ends_mid_line(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let Ok(len) = f.seek(SeekFrom::End(0)) else {
        return false;
    };
    if len == 0 {
        return false;
    }
    if f.seek(SeekFrom::Start(len - 1)).is_err() {
        return false;
    }
    let mut last = [0u8; 1];
    f.read_exact(&mut last).is_ok() && last[0] != b'\n'
}

/// A transcript's lines, as decoded text, in order.
///
/// Splits on `\n` over **bytes** rather than using [`BufRead::lines`]: `lines()`
/// yields `Err` for a line that is not valid UTF-8, and the `map_while(ok)` every
/// reader here uses would then silently drop the ENTIRE rest of the file. A torn
/// write can cut a multi-byte character in half (an interrupted append is not
/// character-aligned), so one damaged line must not be able to truncate a resumed
/// transcript. Decoded lossily and left for the caller to parse-or-skip.
fn text_lines(path: &Path) -> Option<impl Iterator<Item = String>> {
    let file = File::open(path).ok()?;
    Some(
        BufReader::new(file)
            .split(b'\n')
            .map_while(Result::ok)
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
    )
}

/// Read a sub-agent transcript file and fold it into a Vec<Entry> using the
/// SAME reducer as the main transcript, so the on-disk sub-agent record renders
/// identically (tool args + results intact). Best-effort: unparsable lines are skipped.
///
/// The MAIN agent's resume path ([`crate::Session::load_path`]) folds its session
/// jsonl through here too — the transcript is no longer embedded in the `.json`.
pub fn read_transcript(path: &Path) -> Vec<crate::Entry> {
    fold_transcript(path).0
}

/// [`read_transcript`] plus how many non-empty lines had to be skipped because
/// they did not parse (a line torn by a full disk or a crash mid-append). Every
/// intact record before AND after the damage still folds: a resumed transcript is
/// never truncated by one bad line. The count is not logged — it exists so a
/// caller (and the tests) can see salvage happened.
fn fold_transcript(path: &Path) -> (Vec<crate::Entry>, usize) {
    let mut entries = Vec::new();
    let mut skipped = 0usize;
    let Some(lines) = text_lines(path) else {
        return (entries, skipped);
    };
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Record>(&line) else {
            skipped += 1;
            continue;
        };
        // Each record folds through the shared event reducer.
        if let Some(ev) = rec.as_event() {
            crate::apply_event(&mut entries, &ev);
        }
    }
    (entries, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serializes_with_t_tag_and_snake_case() {
        let start = Record::Start {
            model: "m".into(),
            label: "l".into(),
            prompt: "p".into(),
        };
        let s = serde_json::to_string(&start).unwrap();
        assert!(s.contains(r#""t":"start""#), "got {s}");
        // Round-trips.
        assert_eq!(serde_json::from_str::<Record>(&s).unwrap(), start);
        // A log written by an older build carried a `kind` here. Unknown fields
        // are ignored, so those runs still read back rather than being skipped as
        // unparsable lines.
        let legacy = r#"{"t":"start","model":"m","label":"l","kind":"background","prompt":"p"}"#;
        assert_eq!(serde_json::from_str::<Record>(legacy).unwrap(), start);

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
        let mut t = TranscriptLog::create(dir.path(), "001-x").unwrap();
        t.write(&Record::Start {
            model: "m".into(),
            label: "l".into(),
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
        let mut t = TranscriptLog::create(dir.path(), "003-edit").unwrap();
        let args = r#"{"path":"src/lib.rs"}"#;
        let result = "@@ -1 +1 @@\n-old line\n+new line";
        t.write(&Record::Start {
            model: "m".into(),
            label: "edit-task".into(),
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

    #[cfg(unix)]
    #[test]
    fn transcript_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("subagents");
        let _t = TranscriptLog::create(&root, "000-x").unwrap();
        let file_mode = std::fs::metadata(root.join("000-x.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode();
        // The transcript carries the sub-agent's full prompt and output.
        assert_eq!(file_mode & 0o777, 0o600, "transcript must be 0600");
        assert_eq!(dir_mode & 0o777, 0o700, "transcript dir must be 0700");
    }

    /// A sub-agent transcript is a pure fold of the `AgentEvent` stream. This
    /// round-trips an event sequence (Steered + Text + ToolStart/ToolEnd) through
    /// disk and back, asserting the folded transcript reconstructs in log order
    /// with tool args and results intact.
    #[test]
    fn read_transcript_folds_an_event_stream_in_order() {
        use crate::{AgentEvent, EntryKind};
        let dir = tempfile::tempdir().unwrap();
        let mut t = TranscriptLog::create(dir.path(), "004-main").unwrap();

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

    /// A partial write must be *reported* as partial: `write_all` collapses "21
    /// bytes landed, then ENOSPC" into a bare error, and that is exactly the
    /// information the rollback needs.
    #[test]
    fn append_all_reports_how_many_bytes_landed_before_the_error() {
        /// Accepts `cap` bytes in total, then fails the way a full disk does.
        struct FullDisk {
            cap: usize,
            written: usize,
        }
        impl Write for FullDisk {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let room = self.cap.saturating_sub(self.written);
                if room == 0 {
                    return Err(std::io::Error::other("No space left on device"));
                }
                let n = room.min(buf.len());
                self.written += n;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut disk = FullDisk {
            cap: 21,
            written: 0,
        };
        // The real shape of the corruption: 21 bytes of the record on disk.
        assert_eq!(
            append_all(&mut disk, b"{\"t\":\"reasoning\",\"text\":\"x\"}\n"),
            Err(21)
        );

        // Nothing accepted at all is `Err(0)` — the file is still on a boundary.
        let mut none = FullDisk { cap: 0, written: 0 };
        assert_eq!(append_all(&mut none, b"abc"), Err(0));

        // A writer with room takes the whole buffer.
        let mut ok = Vec::new();
        assert_eq!(append_all(&mut ok, b"abc\n"), Ok(()));
        assert_eq!(ok, b"abc\n");
    }

    /// **The bug.** A full disk left 21 bytes of a `Reasoning` record on disk with
    /// no newline (`{"t":"reasoning","tex`), and the next record was appended
    /// straight onto that fragment — one unparsable line, two records lost, and a
    /// line-by-line JSON reader dying on it.
    ///
    /// Reopening a transcript that ends mid-record must start the next record on
    /// its own line, so the damage stays confined to the fragment.
    #[test]
    fn a_torn_tail_does_not_swallow_the_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("005-torn.jsonl");
        let mut t = TranscriptLog::append(&path).unwrap();
        t.write(&Record::Text {
            chunk: "before".into(),
        });
        drop(t);
        // Exactly what the ENOSPC append left behind: a record cut mid-key, no
        // trailing newline.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"{\"t\":\"reasoning\",\"tex").unwrap();
        }

        let mut t = TranscriptLog::append(&path).unwrap();
        t.write(&Record::ToolStart {
            id: "call-1".into(),
            name: "shell".into(),
            args: "{}".into(),
        });
        t.write(&Record::Text {
            chunk: "after".into(),
        });
        t.flush();

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 4, "the fragment is its own line: {body:?}");
        assert_eq!(
            lines[1], "{\"t\":\"reasoning\",\"tex",
            "the fragment stayed a lone broken line"
        );
        for good in [lines[0], lines[2], lines[3]] {
            serde_json::from_str::<Record>(good).expect("every other line is a standalone Record");
        }

        // Read-back salvage: exactly one line skipped, and BOTH the records before
        // and after the damage fold.
        let (entries, skipped) = fold_transcript(&path);
        assert_eq!(skipped, 1, "only the fragment is lost");
        let text: String = entries
            .iter()
            .filter_map(|e| match &e.kind {
                crate::EntryKind::Assistant(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("before") && text.contains("after"),
            "records on both sides of the tear survive: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.kind, crate::EntryKind::Tool { name, .. } if name == "shell")),
            "the record that followed the fragment is no longer swallowed: {entries:?}"
        );
    }

    /// One serialized writer per file: every append goes through the one
    /// `Mutex`-held handle, so two tasks hammering the same transcript can never
    /// split each other's lines. Each event is a single buffered write including
    /// its newline — the invariant a torn line would break.
    #[test]
    fn concurrent_writers_on_one_handle_never_tear_a_line() {
        use std::sync::Arc;
        const PER_THREAD: usize = 200;
        let dir = tempfile::tempdir().unwrap();
        let writer = Arc::new(std::sync::Mutex::new(
            TranscriptLog::create(dir.path(), "008-race").unwrap(),
        ));
        let path = dir.path().join("008-race.jsonl");

        let handles: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|tag| {
                let writer = Arc::clone(&writer);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        // Long payloads: a big line is what needs more than one
                        // write syscall, and so what tears.
                        let chunk = format!("{tag}-{i}-{}", "x".repeat(4096));
                        let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
                        w.write(&Record::Text { chunk });
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            PER_THREAD * 2,
            "one line per record, no splits"
        );
        let mut seen = std::collections::HashSet::new();
        for l in &lines {
            match serde_json::from_str::<Record>(l).expect("every line parses standalone") {
                Record::Text { chunk } => {
                    seen.insert(chunk);
                }
                other => panic!("unexpected record {other:?}"),
            }
        }
        for tag in ["a", "b"] {
            for i in 0..PER_THREAD {
                let want = format!("{tag}-{i}-{}", "x".repeat(4096));
                assert!(seen.contains(&want), "missing {tag}-{i}");
            }
        }
    }

    /// Read a transcript's records back as written, skipping blank lines.
    fn records(path: &Path) -> Vec<Record> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Record>(l).expect("standalone Record"))
            .collect()
    }

    /// The IO the coalescing exists to remove: a token-sized delta must not cost
    /// a line (and a `write(2)`) of its own.
    #[test]
    fn streaming_deltas_coalesce_into_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("009-coalesce.jsonl");
        let mut t = TranscriptLog::append(&path).unwrap();
        for word in ["The ", "quick ", "brown ", "fox"] {
            t.write(&Record::Text { chunk: word.into() });
        }
        t.flush();
        assert_eq!(
            records(&path),
            vec![Record::Text {
                chunk: "The quick brown fox".into()
            }],
            "four deltas, one line"
        );
    }

    /// The boundaries the coalescing must not blur: reasoning giving way to
    /// output, and a tool call opening and closing mid-stream. Each is a distinct
    /// record on disk, in call order.
    #[test]
    fn a_kind_change_or_a_tool_call_breaks_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("010-boundary.jsonl");
        let mut t = TranscriptLog::append(&path).unwrap();
        t.write(&Record::Reasoning {
            text: "think".into(),
        });
        t.write(&Record::Reasoning { text: "ing".into() });
        t.write(&Record::Text {
            chunk: "say".into(),
        });
        t.write(&Record::Text {
            chunk: "ing".into(),
        });
        t.write(&Record::ToolStart {
            id: "c1".into(),
            name: "read".into(),
            args: "{}".into(),
        });
        t.write(&Record::ToolEnd {
            id: "c1".into(),
            name: "read".into(),
            result: "ok".into(),
            ok: true,
        });
        t.write(&Record::Text {
            chunk: "after".into(),
        });
        t.flush();

        assert_eq!(
            records(&path),
            vec![
                Record::Reasoning {
                    text: "thinking".into()
                },
                Record::Text {
                    chunk: "saying".into()
                },
                Record::ToolStart {
                    id: "c1".into(),
                    name: "read".into(),
                    args: "{}".into(),
                },
                Record::ToolEnd {
                    id: "c1".into(),
                    name: "read".into(),
                    result: "ok".into(),
                    ok: true,
                },
                Record::Text {
                    chunk: "after".into()
                },
            ],
        );
    }

    /// Two tools streaming at once must not have their output merged: the fold
    /// pushes a `ToolOutput` onto the call named by its `id`, so one merged record
    /// would append both tools' output to whichever id won.
    #[test]
    fn output_from_two_tools_never_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("011-two-tools.jsonl");
        let mut t = TranscriptLog::append(&path).unwrap();
        for (id, chunk) in [("a", "a1"), ("a", "a2"), ("b", "b1"), ("a", "a3")] {
            t.write(&Record::ToolOutput {
                id: id.into(),
                chunk: chunk.into(),
            });
        }
        t.flush();
        assert_eq!(
            records(&path),
            vec![
                Record::ToolOutput {
                    id: "a".into(),
                    chunk: "a1a2".into()
                },
                Record::ToolOutput {
                    id: "b".into(),
                    chunk: "b1".into()
                },
                Record::ToolOutput {
                    id: "a".into(),
                    chunk: "a3".into()
                },
            ],
        );
    }

    /// Coalescing is only sound because it is invisible to the reader: the same
    /// event stream must fold to the same transcript whether it reached disk as
    /// one record per delta or as one record per run of deltas. Compares the
    /// folded file against `apply_event` over the raw events.
    #[test]
    fn a_coalesced_transcript_folds_exactly_like_the_event_stream() {
        use crate::AgentEvent;
        let events = vec![
            AgentEvent::Steered("do the thing".into()),
            AgentEvent::Reasoning("first ".into()),
            AgentEvent::Reasoning("I plan".into()),
            AgentEvent::Text("Now ".into()),
            AgentEvent::Text("editing".into()),
            AgentEvent::ToolStart {
                id: "c1".into(),
                name: "shell".into(),
                args: r#"{"cmd":"ls"}"#.into(),
            },
            AgentEvent::ToolOutput {
                id: "c1".into(),
                chunk: "src\n".into(),
            },
            AgentEvent::ToolOutput {
                id: "c1".into(),
                chunk: "Cargo.toml\n".into(),
            },
            AgentEvent::ToolEnd {
                id: "c1".into(),
                name: "shell".into(),
                result: "src\nCargo.toml\n".into(),
                ok: true,
            },
            AgentEvent::Text("Done".into()),
        ];

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("012-fold.jsonl");
        {
            let mut t = TranscriptLog::append(&path).unwrap();
            for ev in &events {
                t.write(&Record::from_event(ev).unwrap());
            }
            // No explicit flush: the writer going out of scope owes the file its
            // last buffered deltas, and that is exactly a run ending mid-stream.
        }
        // Coalescing happened at all — otherwise this asserts nothing.
        assert!(
            records(&path).len() < events.len(),
            "deltas were merged: {:?}",
            records(&path)
        );

        let mut want = Vec::new();
        for ev in &events {
            crate::apply_event(&mut want, ev);
        }
        let got = read_transcript(&path);
        let kinds = |v: &[crate::Entry]| v.iter().map(|e| e.kind.clone()).collect::<Vec<_>>();
        assert_eq!(kinds(&got), kinds(&want), "same fold, fewer lines");
    }

    /// A stream long enough to matter still lands in pieces: the size threshold
    /// bounds how much streamed text a crash can take with it, and a delta that
    /// is already big enough to amortize its own framing is never held back.
    #[test]
    fn a_long_stream_lands_in_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("013-long.jsonl");
        let mut t = TranscriptLog::append(&path).unwrap();
        for _ in 0..500 {
            t.write(&Record::Text {
                chunk: "tok ".into(),
            });
        }
        let lines = records(&path);
        assert!(lines.len() > 1, "not one giant line at the end");
        for rec in &lines {
            let len = delta_len(rec).unwrap();
            assert!(
                len < COALESCE_BYTES + 4,
                "a line holds about one threshold of payload, got {len}"
            );
        }

        // One oversized delta needs no partner: on an empty buffer it lands on
        // its own, at once. (Onto an OPEN buffer it still merges first — same
        // stream, one line, one syscall.)
        t.flush();
        let big = "x".repeat(COALESCE_BYTES * 2);
        t.write(&Record::Text { chunk: big.clone() });
        assert_eq!(
            records(&path).last().unwrap(),
            &Record::Text { chunk: big },
            "an oversized delta is written straight through"
        );
    }

    #[test]
    fn open_error_is_returned_not_panicked() {
        // A path whose parent cannot be created (a file where a dir is needed).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let bad_dir = blocker.join("subdir"); // parent is a file
        assert!(TranscriptLog::create(&bad_dir, "id").is_err());
    }

    /// A run owns its file. The dir is keyed by session id and survives a resume,
    /// but the id counter restarts at 0 each process — so without exclusive
    /// creation a resumed session's first task would append onto the previous
    /// run's log, producing a file with two `Start`s and making an orphaned run
    /// look complete.
    #[test]
    fn create_refuses_to_reuse_an_existing_run_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = TranscriptLog::create(dir.path(), "000-sub-task").unwrap();
        first.write(&Record::Start {
            model: "m".into(),
            label: "sub-task".into(),
            prompt: "first run".into(),
        });
        // No End: the first run crashed. It must stay an identifiable orphan.
        drop(first);

        let err = match TranscriptLog::create(dir.path(), "000-sub-task") {
            Ok(_) => panic!("an id already on disk must not be reopened"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let path = dir.path().join("000-sub-task.jsonl");
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1, "untouched by the second attempt");
    }

    /// A tear is not character-aligned, so a torn line can hold half a multi-byte
    /// character. `BufRead::lines()` yields `Err` for that, and the readers'
    /// `map_while(ok)` dropped the ENTIRE rest of the file — a resumed session
    /// silently lost every record after the damage.
    #[test]
    fn read_survives_a_line_that_is_not_valid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("006-mojibake.jsonl");
        let mut raw = Vec::new();
        raw.extend_from_slice(br#"{"t":"text","chunk":"first"}"#);
        raw.push(b'\n');
        // A record cut in the middle of a 3-byte character.
        raw.extend_from_slice(b"{\"t\":\"text\",\"chunk\":\"\xe2\x82\n");
        raw.extend_from_slice(br#"{"t":"text","chunk":"second"}"#);
        raw.push(b'\n');
        raw.extend_from_slice(br#"{"t":"end","status":"ok","bytes":0}"#);
        raw.push(b'\n');
        std::fs::write(&path, &raw).unwrap();

        let (entries, skipped) = fold_transcript(&path);
        assert_eq!(skipped, 1);
        let text: String = entries
            .iter()
            .filter_map(|e| match &e.kind {
                crate::EntryKind::Assistant(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("first") && text.contains("second"),
            "the read did not stop at the bad line: {entries:?}"
        );
    }
}

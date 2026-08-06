use std::io::Read;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext};

use super::{DEFAULT_READ_LIMIT, MAX_LINE, MAX_READ_BYTES};

// ---- read ----

pub struct ReadTool;

#[derive(Deserialize)]
struct ReadArgs {
    // Accept the names other agents' read tools use, so a model trained on
    // `file_path` (Claude's native Read), `file`, etc. still lands the call
    // instead of erroring on a "missing field `path`".
    #[serde(
        alias = "file_path",
        alias = "filepath",
        alias = "file",
        alias = "filename",
        alias = "file_name",
        alias = "path_to_file"
    )]
    path: String,
    // Common synonyms for the paging window, for the same reason.
    #[serde(default, alias = "start", alias = "start_line", alias = "line")]
    offset: Option<usize>,
    #[serde(
        default,
        alias = "count",
        alias = "lines",
        alias = "num_lines",
        alias = "max_lines"
    )]
    limit: Option<usize>,
    /// Read the whole file WITHOUT clipping long lines AND without the per-call
    /// output budget (ignoring `offset`/`limit`), so it can be marked fully read —
    /// the escape hatch for a full rewrite via `write`, whether the obstacle is a
    /// line over `MAX_LINE` bytes or simply a file larger than the output budget.
    /// Returns the whole content (bounded only by the 50 MB file cap), so it costs
    /// tokens; opt-in.
    #[serde(default, alias = "raw", alias = "whole", alias = "no_clip")]
    full: bool,
}

/// One-pass window extractor over a file's bytes: captures exactly the lines
/// [`start`, `start + limit)` (1-based; `limit == None` = through EOF) and
/// counts the file's total `\n`s, without ever building the whole text or
/// validating the whole file's UTF-8. Feed it chunks; [`WindowScanner::finish`]
/// returns the window bytes and the newline count.
///
/// The window is always cut at a line boundary (just past a `\n`, or at EOF),
/// so rendering the window with `str::lines()` matches rendering the same
/// lines of the whole file — the `\r`-stripping and empty-line rules carry
/// over unchanged.
struct WindowScanner {
    /// 1-based first line to capture.
    start: usize,
    /// How many lines to capture; `None` = through EOF.
    limit: Option<usize>,
    /// The captured bytes, exactly the requested window.
    window: Vec<u8>,
    /// Inside the window (`start == 1` begins capturing at byte 0).
    capturing: bool,
    /// The `limit`-th window newline has been seen — stop appending, but keep
    /// counting newlines for the total.
    window_done: bool,
    /// `\n` seen so far in the whole file.
    newlines: usize,
    /// `\n` seen while capturing — bounds the window.
    window_newlines: usize,
}

impl WindowScanner {
    fn new(start: usize, limit: Option<usize>) -> Self {
        Self {
            start,
            limit,
            window: Vec::new(),
            capturing: start == 1,
            // `limit: Some(0)` captures nothing.
            window_done: limit == Some(0),
            newlines: 0,
            window_newlines: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        loop {
            let Some(j) = rest.iter().position(|&b| b == b'\n') else {
                // No newline left in this chunk: the bytes belong to the
                // current line, captured only inside the window.
                if self.capturing && !self.window_done {
                    self.window.extend_from_slice(rest);
                }
                break;
            };
            // This is the `self.newlines`-th newline of the file (1-based).
            self.newlines += 1;
            if !self.capturing && self.newlines == self.start - 1 {
                // The (start-1)-th newline ends line `start - 1`: the window
                // opens just past it, with line `start`. Nothing of line
                // `start - 1` is captured — including its newline.
                self.capturing = true;
            } else if self.capturing && !self.window_done {
                // A complete window line: content up to and including its
                // newline.
                self.window.extend_from_slice(&rest[..=j]);
                self.window_newlines += 1;
                if self.limit.is_some_and(|l| self.window_newlines >= l) {
                    self.window_done = true;
                }
            }
            rest = &rest[j + 1..];
        }
    }

    fn finish(self) -> (Vec<u8>, usize) {
        (self.window, self.newlines)
    }
}

#[async_trait]
impl Tool for ReadTool {
    /// The file itself — in an audit, provenance per byte is the whole point.
    fn output_source(&self, args: &serde_json::Value) -> String {
        args.get("path")
            .and_then(|v| v.as_str())
            .map(|p| format!("file {p}"))
            .unwrap_or_else(|| "file".to_string())
    }
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read a file from disk (50 MB cap). Returns 1-based line-numbered content (the `N: ` \
         prefix is display-only — never include it in edit strings). Use `offset`/`limit` to \
         page through large files; paging accumulates, so reading a file start-to-finish \
         marks it fully read (then `write`/`delete` are allowed). A read that doesn't yet \
         cover the whole file — `offset`/`limit` short of EOF, or any line over 2000 bytes \
         (clipped) — marks the file partially-read; \
         `edit` still works against it, but `write` refuses to overwrite a file that \
         hasn't been read in full. You must read a file before editing it. To rewrite a large \
         file, or one with a line over 2000 bytes (which a normal read clips every time so it \
         can never be marked fully read), read it once with `full: true` (whole file, no \
         clipping and no output-size cap) first."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to cwd."},
                "offset": {"type": "integer", "default": 1, "description": "1-based line to start at (default 1)."},
                "limit": {"type": "integer", "default": DEFAULT_READ_LIMIT, "description": "Max lines to return (default 2000)."},
                "full": {"type": "boolean", "default": false, "description": "Read the entire file with NO line clipping and NO output-size cap (ignores offset/limit); returns the whole file, bounded only by the 50 MB load cap. Use it to fully read a file — large, or with a very long line — so a subsequent `write` rewrite is accepted. Costs more tokens. Default false."}
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: ReadArgs = crate::tool_args("read", args).map_err(|e| {
            // Append the exact shape so a malformed call self-corrects on the next
            // try rather than guessing at what was wrong.
            anyhow::anyhow!(
                "{e}\nread expects {{\"path\": \"<file>\" (required), \
                 \"offset\": <1-based start line, optional>, \"limit\": <max lines, optional>}}. \
                 The path may also be given as \"file_path\"."
            )
        })?;
        let path = ctx.resolve_read(&a.path)?;

        // The window to read: `start` (1-based) and how many lines to capture.
        // A `full` read captures through EOF and renders without clipping or
        // budget — the escape hatch for a `write` rewrite.
        let start = if a.full {
            1
        } else {
            a.offset.unwrap_or(1).max(1)
        };
        let window_limit = if a.full {
            None
        } else {
            Some(a.limit.unwrap_or(DEFAULT_READ_LIMIT))
        };

        // Open + guards + windowed scan on the blocking pool: this is `std::fs`
        // on a handle that can be a multi-MB read, so it must not occupy a tokio
        // worker. The closure takes an owned copy of the resolved path (no borrow
        // of `ctx` or `path` across the `spawn_blocking` boundary) and returns
        // the window's text and the file's total line count; the guards and the
        // size cap run inside it, with the same errors.
        let resolved = path.clone();
        let (text, total_lines) = tokio::task::spawn_blocking(move || -> Result<(String, usize)> {
            // Open the file first so the handle is fixed before any path resolution —
            // this closes the TOCTOU window between secret-file validation and reading.
            let mut file = std::fs::File::open(&resolved)
                .with_context(|| format!("opening {}", resolved.display()))?;

            // Validate the path is not a secret file.
            crate::guard_secret_read(&resolved)?;

            // Prove the handle we opened is still the object this path names — if any
            // component was swapped between the open and the guard above, reject it.
            // Enforced on every platform (unix via dev/ino, Windows via the file
            // index), so the guard is not quietly weaker on one of them.
            crate::guard_not_swapped(&file, &resolved)?;

            // Check file size from the open handle (not a separate stat).
            let file_len = file
                .metadata()
                .with_context(|| format!("statting {}", resolved.display()))?
                .len();
            if file_len > MAX_READ_BYTES {
                bail!(
                    "{} is {} bytes, over this tool's {MAX_READ_BYTES}-byte cap — it's too large to \
                     load whole; use `grep` to search it or `bash` (`sed`/`head`/`tail`) to slice out \
                     the range you need",
                    resolved.display(),
                    file_len
                );
            }

            // One byte pass over the file: capture only the window and count
            // newlines for the total. No whole-file String, no whole-file UTF-8
            // validation, no second `lines()` pass — paging a large file used
            // to pay all three per page. The window itself must still be text
            // (a `full` read's window *is* the whole file, so it keeps the old
            // whole-file validation); a binary tail outside the window no
            // longer fails a paged read of the text before it, and errors when
            // a page actually reaches it.
            let mut scanner = WindowScanner::new(start, window_limit);
            let mut buf = vec![0u8; 64 * 1024];
            let mut last_byte = 0u8;
            loop {
                let n = file
                    .read(&mut buf)
                    .with_context(|| format!("reading {}", resolved.display()))?;
                if n == 0 {
                    break;
                }
                last_byte = buf[n - 1];
                scanner.feed(&buf[..n]);
            }
            let (window, newlines) = scanner.finish();
            let total_lines = if file_len == 0 {
                0
            } else {
                // `str::lines()` semantics: a trailing newline does not open a
                // final empty line.
                newlines + 1 - usize::from(last_byte == b'\n')
            };
            let text = match String::from_utf8(window) {
                Ok(t) => t,
                Err(_) => {
                    bail!(
                        "{} is not a text file (invalid UTF-8{}) — this tool only reads text; \
                         inspect binaries via bash (`file`, `hexdump -C`, `strings`) if needed",
                        resolved.display(),
                        if a.full { "" } else { " in the requested lines" }
                    );
                }
            };
            Ok((text, total_lines))
        })
        .await??;
        // `full` reads the whole file with no per-line clip and no output budget
        // (ignoring offset/limit), so a file with a line over `MAX_LINE` or one
        // simply larger than the budget can still be read whole and marked fully
        // read — the legitimate path to a `write` rewrite. A normal read pages with
        // offset/limit, clips over-long lines, and stops at the read budget.
        //
        // The read budget is a generous multiple of the shared tool-output cap
        // (`ctx.max_output`, which is sized for taming *unbounded* output —
        // build walls, huge greps). A file read is different: the model asked for
        // this content, and often needs the whole file (or is reading an output a
        // `shell`/`grep`/`git` overflow just spilled to disk), so reads get far
        // more room — see `READ_BUDGET_FACTOR`.
        let read_budget = ctx.max_output.saturating_mul(super::READ_BUDGET_FACTOR);
        let mut out = String::new();
        let mut any_line_truncated = false;
        // The last line number actually emitted (0 = none) and whether the read
        // stopped at the budget rather than EOF/limit — drives the coverage record
        // and the "more to read" hint.
        let mut last_line = start.saturating_sub(1);
        let mut budget_stopped = false;
        // `text` holds exactly the requested window, so its lines are numbered
        // from `start` directly — no skip, no take, and no whole-file scan.
        for (j, line) in text.lines().enumerate() {
            let n = start + j;
            let cut = if a.full {
                line.len()
            } else {
                crate::floor_char_boundary(line, MAX_LINE)
            };
            if cut < line.len() {
                any_line_truncated = true;
            }
            let rendered = format!("{n:>6}: {}\n", &line[..cut]);
            // A normal read stops at the budget on a line boundary, so the model
            // sees whole lines and the recorded coverage is exact. Always emit at
            // least the first line, however long, so a read never returns nothing
            // useful.
            if !a.full && !out.is_empty() && out.len() + rendered.len() > read_budget {
                budget_stopped = true;
                break;
            }
            out.push_str(&rendered);
            last_line = n;
        }
        if out.is_empty() {
            out.push_str("(file is empty or offset past end)");
        }
        // Record what was seen. A `full` read (or an authored file) is fully known;
        // a normal read records its `[start, last_line]` range, which accumulates
        // across pages so a file read start-to-finish becomes fully read. A clipped
        // line keeps it partial until a `full` read sees that line whole.
        if a.full {
            ctx.mark_read(&path);
        } else {
            ctx.record_read(&path, start, last_line, total_lines, any_line_truncated);
        }
        // Tell the model when there's more to read, and how to get it.
        if !a.full && last_line < total_lines {
            out.push_str(&format!(
                "\n… [showing lines {start}–{last_line} of {total_lines}{}; \
                 read with offset {} to continue, or full: true for the whole file]",
                if budget_stopped {
                    " (stopped at the output budget)"
                } else {
                    ""
                },
                last_line + 1
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_allows_outside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("notes.txt");
        std::fs::write(&target, "data").unwrap();

        let ctx = ToolContext::new(cwd.path().to_path_buf());
        let out = ReadTool
            .execute(serde_json::json!({"path": target.to_str().unwrap()}), &ctx)
            .await
            .expect("reads are not confined to cwd");
        assert!(out.contains("data"), "got: {out}");
    }

    /// The alias set at the deserialization seam, including the `file` spelling
    /// models reach for most often after `file_path`.
    #[test]
    fn read_args_accept_path_aliases_and_reject_a_doubled_path() {
        let a: ReadArgs = serde_json::from_value(json!({"file": "x"})).unwrap();
        assert_eq!(a.path, "x");
        let a: ReadArgs = serde_json::from_value(json!({"path": "x"})).unwrap();
        assert_eq!(a.path, "x");
        // Pinned serde behavior: a canonical field *and* one of its aliases in
        // the same object is a duplicate-field error, not last-wins — so an
        // ambiguous call fails loudly instead of silently picking one path.
        let err = match serde_json::from_value::<ReadArgs>(json!({"path": "a", "file": "b"})) {
            Ok(a) => panic!("a doubled path should not deserialize (got {})", a.path),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("duplicate field"), "{err}");
    }

    /// A model trained on `file_path` (Claude's native Read) — or another common
    /// alias — still lands the call instead of erroring on a missing `path`.
    #[tokio::test]
    async fn read_accepts_file_path_and_other_path_aliases() {
        let cwd = tempfile::tempdir().unwrap();
        let target = cwd.path().join("notes.txt");
        std::fs::write(&target, "line one\nline two\nline three\n").unwrap();
        let ctx = ToolContext::new(cwd.path().to_path_buf());

        for key in ["file_path", "filepath", "file", "filename", "path_to_file"] {
            let out = ReadTool
                .execute(serde_json::json!({ key: target.to_str().unwrap() }), &ctx)
                .await
                .unwrap_or_else(|e| panic!("alias {key:?} should resolve to path: {e}"));
            assert!(
                out.contains("line one"),
                "alias {key:?} read the file: {out}"
            );
        }

        // `offset`/`limit` synonyms page the same way.
        let out = ReadTool
            .execute(
                serde_json::json!({"file_path": target.to_str().unwrap(), "start": 2, "count": 1}),
                &ctx,
            )
            .await
            .expect("offset/limit synonyms resolve");
        assert!(
            out.contains("line two") && !out.contains("line one"),
            "paged: {out}"
        );
    }

    /// `full: true` returns the whole file with no per-line clipping, so a file
    /// with a line over `MAX_LINE` bytes comes back intact and is marked fully
    /// read — where a normal read clips that line and records it partial.
    #[tokio::test]
    async fn full_read_does_not_clip_long_lines_and_marks_complete() {
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("big.txt");
        let long = "y".repeat(MAX_LINE + 300);
        std::fs::write(&path, format!("{long}\n")).unwrap();
        let ctx = ToolContext::new(cwd.path().to_path_buf());

        // A normal read clips the long line and records the file partial.
        let clipped = ReadTool
            .execute(serde_json::json!({"path": "big.txt"}), &ctx)
            .await
            .unwrap();
        assert!(
            !clipped.contains(&long),
            "a normal read clips the long line"
        );
        assert_eq!(ctx.read_state(&path), crate::ReadState::Partial);

        // `full: true` returns the whole line and marks the file complete.
        let whole = ReadTool
            .execute(serde_json::json!({"path": "big.txt", "full": true}), &ctx)
            .await
            .unwrap();
        assert!(
            whole.contains(&long),
            "full read returns the whole line: {whole}"
        );
        assert_eq!(ctx.read_state(&path), crate::ReadState::Fresh);
    }

    /// `full: true` also bypasses the per-call output budget (not just line
    /// clipping), so a file merely LARGER than the budget — no long line — can be
    /// read whole and marked complete, where a normal read caps out partial. This
    /// is what lets `write` rewrite a large file at all.
    #[tokio::test]
    async fn full_read_bypasses_the_output_budget() {
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("big.txt");
        // Many normal lines, comfortably over a small output budget.
        let body: String = (0..500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let mut ctx = ToolContext::new(cwd.path().to_path_buf());
        // Small budget so the 500-line file overflows even the 20x read budget
        // (200 * 20 = 4000 bytes, well under the file's rendered size).
        ctx.max_output = 200;

        // A single normal read stops at the budget and is recorded partial.
        let paged = ReadTool
            .execute(serde_json::json!({"path": "big.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(ctx.read_state(&path), crate::ReadState::Partial);
        assert!(
            paged.contains("stopped at the output budget"),
            "the read hints there's more: {paged}"
        );

        // Full read returns everything and marks the file complete.
        let whole = ReadTool
            .execute(serde_json::json!({"path": "big.txt", "full": true}), &ctx)
            .await
            .unwrap();
        assert!(
            whole.contains("line 499"),
            "full read returns the whole file, past the budget"
        );
        assert_eq!(ctx.read_state(&path), crate::ReadState::Fresh);
    }

    /// The paging contract the model relies on: reading a big file start-to-finish
    /// with `offset`/`limit` accumulates coverage, so once the last page lands the
    /// file is fully read and `write`/`delete` are unblocked — no `full` needed.
    #[tokio::test]
    async fn paging_start_to_finish_marks_the_file_fully_read() {
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("big.txt");
        // 1000 short lines, no over-long line.
        let body: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let ctx = ToolContext::new(cwd.path().to_path_buf());

        let page = |off: usize, lim: usize| serde_json::json!({"path": "big.txt", "offset": off, "limit": lim});
        // Page 1: lines 1–400 → still partial (tail unseen).
        ReadTool.execute(page(1, 400), &ctx).await.unwrap();
        assert_eq!(ctx.read_state(&path), crate::ReadState::Partial);
        // Page 2: lines 401–800 → contiguous, still short of the end.
        ReadTool.execute(page(401, 400), &ctx).await.unwrap();
        assert_eq!(ctx.read_state(&path), crate::ReadState::Partial);
        // Page 3: lines 801–1000 → coverage now spans the whole file → fully read.
        ReadTool.execute(page(801, 400), &ctx).await.unwrap();
        assert_eq!(ctx.read_state(&path), crate::ReadState::Fresh);
    }

    /// Coverage is the contiguous run from line 1: paging that leaves a GAP is not
    /// fully read (a skipped middle is genuinely unseen), and an out-of-order tail
    /// read does not count until the prefix reaches it — so completing means
    /// reading contiguously through the gap to the end.
    #[tokio::test]
    async fn paging_with_a_gap_stays_partial_until_covered_contiguously() {
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("big.txt");
        let body: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let ctx = ToolContext::new(cwd.path().to_path_buf());
        let page = |off: usize, lim: usize| serde_json::json!({"path": "big.txt", "offset": off, "limit": lim});

        ReadTool.execute(page(1, 400), &ctx).await.unwrap(); // 1–400
        ReadTool.execute(page(601, 400), &ctx).await.unwrap(); // 601–1000, skips 401–600
        assert_eq!(
            ctx.read_state(&path),
            crate::ReadState::Partial,
            "a gap in coverage is not fully read"
        );
        // Reading from the gap through the end extends the contiguous run to EOF.
        ReadTool.execute(page(401, 600), &ctx).await.unwrap(); // 401–1000
        assert_eq!(ctx.read_state(&path), crate::ReadState::Fresh);
    }

    /// The scanner's total and window must match `str::lines()` exactly —
    /// trailing newlines, empty lines and `\r\n` included — for any window and
    /// any chunking. This is the correctness net for the byte-level scan; the
    /// total feeds the coverage record and the "of Z" hint, the window feeds
    /// the renderer.
    #[test]
    fn window_scan_matches_str_lines_semantics() {
        let cases: &[&[u8]] = &[
            b"",
            b"a",
            b"a\n",
            b"a\nb",
            b"a\nb\n",
            b"\n",
            b"a\n\nb",
            b"a\n\nb\n",
            b"a\r\nb",
            b"a\r\nb\r\n",
            b"\r\n",
            b"a\nb\nc\nd\ne",
            b"line one\nline two\nline three\n",
        ];
        for bytes in cases {
            let text = std::str::from_utf8(bytes).unwrap();
            let want_total = text.lines().count();
            for start in 1..=(want_total + 2) {
                for limit in [Some(0), Some(1), Some(2), Some(5), None] {
                    // Feed several chunk sizes: 1-byte chunks stress the
                    // cross-chunk state machine, while a single whole chunk
                    // stresses a window opening mid-chunk (the path that
                    // historically double-captured the remainder).
                    for chunk in [1usize, 3, bytes.len().max(1)] {
                        let mut sc = WindowScanner::new(start, limit);
                        for slice in bytes.chunks(chunk) {
                            sc.feed(slice);
                        }
                        let (window, newlines) = sc.finish();
                        let total = if bytes.is_empty() {
                            0
                        } else {
                            newlines + 1 - usize::from(bytes.last() == Some(&b'\n'))
                        };
                        assert_eq!(
                            total, want_total,
                            "total for {bytes:?} start={start} limit={limit:?} chunk={chunk}"
                        );
                        let want: Vec<&str> = text
                            .lines()
                            .skip(start - 1)
                            .take(limit.unwrap_or(usize::MAX))
                            .collect();
                        let got_text = String::from_utf8(window).unwrap();
                        let got: Vec<&str> = got_text.lines().collect();
                        assert_eq!(
                            got, want,
                            "window for {bytes:?} start={start} limit={limit:?} chunk={chunk}"
                        );
                    }
                }
            }
        }
    }

    /// The windowed-read contract this change pins: a paged read validates
    /// only the window it returns. A file whose first lines are text but whose
    /// tail is binary now pages fine — the old whole-file `read_to_string`
    /// failed it on InvalidData — and errors only when a page reaches the
    /// binary.
    #[tokio::test]
    async fn windowed_read_no_longer_requires_the_whole_file_to_be_text() {
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("mixed.txt");
        let mut body = b"line one\nline two\n".to_vec();
        body.extend_from_slice(&[0xff, 0xfe, 0x80]); // invalid UTF-8
        std::fs::write(&path, &body).unwrap();
        let ctx = ToolContext::new(cwd.path().to_path_buf());

        let out = ReadTool
            .execute(
                serde_json::json!({"path": "mixed.txt", "offset": 1, "limit": 1}),
                &ctx,
            )
            .await
            .unwrap_or_else(|e| panic!("a paged read of the text prefix must not fail: {e:#}"));
        assert!(out.contains("line one"), "{out}");

        // A page that actually reaches the binary still errors, as before.
        let err = ReadTool
            .execute(
                serde_json::json!({"path": "mixed.txt", "offset": 3, "limit": 5}),
                &ctx,
            )
            .await
            .expect_err("a page over the invalid UTF-8 must fail");
        assert!(err.to_string().contains("not a text file"), "{err}");
    }

    /// A call with no path at all gets an instructive error naming the exact
    /// shape, not just a bare "missing field `path`".
    #[tokio::test]
    async fn read_without_a_path_explains_the_expected_shape() {
        let ctx = ToolContext::new(tempfile::tempdir().unwrap().path().to_path_buf());
        let err = ReadTool
            .execute(serde_json::json!({"offset": 10, "limit": 5}), &ctx)
            .await
            .expect_err("a path-less read must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("\"path\""), "names the required field: {msg}");
        assert!(msg.contains("file_path"), "mentions the alias: {msg}");
    }

    /// In `Read` mode the readable roots are the cwd, the scratch dir and the
    /// tool-output dir: a read outside them is refused with the read string, one
    /// under the cwd goes through.
    ///
    /// The "outside" probe is a *sibling tempdir*, not `/etc/hostname`: a
    /// unix-style absolute path is not absolute on Windows, so `resolve_under`
    /// joins it onto the cwd's drive (`C:/etc/hostname`) and the refusal names
    /// that instead — the check still fires, but the assertion on its text
    /// cannot be written portably. A sibling tempdir is absolute on every
    /// platform and outside every readable root (they are all deeper than the
    /// temp dir), so this exercises the same refusal hermetically.
    #[tokio::test]
    async fn read_and_search_refuse_outside_roots_in_strict_mode() {
        let cwd = tempfile::tempdir().unwrap();
        let ctx = crate::sandbox::confined_ctx(cwd.path(), crate::SandboxMode::Jail);

        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("hostname");
        std::fs::write(&outside_file, "not yours").unwrap();
        let err = ReadTool
            .execute(serde_json::json!({"path": outside_file}), &ctx)
            .await
            .expect_err("a strict-mode agent may not read outside its roots")
            .to_string();
        assert!(
            err.contains(&format!(
                "sandbox: refusing to read {}",
                outside_file.display()
            )),
            "{err}"
        );
        assert!(err.contains("strictly confined and may read only"), "{err}");
        assert!(
            err.contains(
                &crate::canonicalize_nearest(cwd.path())
                    .display()
                    .to_string()
            ),
            "the refusal must name the readable root: {err}"
        );

        std::fs::write(cwd.path().join("notes.txt"), "data").unwrap();
        let out = ReadTool
            .execute(serde_json::json!({"path": "notes.txt"}), &ctx)
            .await
            .expect("reads under the cwd root are allowed");
        assert!(out.contains("data"), "got: {out}");
    }

    /// The file-open + read path runs on the blocking pool (`spawn_blocking`): a
    /// big file's whole-file `read_to_string` must not occupy a tokio worker.
    /// Whether the work actually landed on a blocking thread is not portably
    /// assertable (the blocking pool is a tokio implementation detail, and a
    /// single-threaded test runtime offers no observable difference), so pin the
    /// observable instead: a read of a large file completes and returns the
    /// right content through the spawned path.
    #[tokio::test]
    async fn read_large_file_completes_via_the_blocking_pool() {
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("big.txt");
        // Enough lines that the whole-file read is non-trivial; the first lines
        // would only survive if the closure actually returned the content.
        let body: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let ctx = ToolContext::new(cwd.path().to_path_buf());

        let out = ReadTool
            .execute(
                serde_json::json!({"path": "big.txt", "offset": 1, "limit": 3}),
                &ctx,
            )
            .await
            .expect("a large read completes");
        assert!(out.contains("line 0"), "{out}");
        assert!(out.contains("line 2"), "{out}");
    }
}

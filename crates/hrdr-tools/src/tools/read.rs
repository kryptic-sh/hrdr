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

#[async_trait]
impl Tool for ReadTool {
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
                "offset": {"type": "integer", "description": "1-based line to start at (default 1)."},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)."},
                "full": {"type": "boolean", "description": "Read the entire file with NO line clipping and NO output-size cap (ignores offset/limit); returns the whole file, bounded only by the 50 MB load cap. Use it to fully read a file — large, or with a very long line — so a subsequent `write` rewrite is accepted. Costs more tokens. Default false."}
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
        let path = ctx.resolve(&a.path);

        // Open the file first so the handle is fixed before any path resolution —
        // this closes the TOCTOU window between secret-file validation and reading.
        let mut file =
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;

        // Validate the path is not a secret file.
        crate::guard_secret_read(&path)?;

        // On Unix, prove the opened descriptor is the same object that
        // canonicalization validated. If any path component was swapped between
        // open and validation, reject it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = file.metadata()?;
            let canon = crate::canonicalize_nearest(&path);
            let validated = std::fs::metadata(&canon)
                .with_context(|| format!("statting canonical {}", canon.display()))?;
            if opened.dev() != validated.dev() || opened.ino() != validated.ino() {
                bail!(
                    "{} changed while it was being validated — re-read the file",
                    path.display()
                );
            }
        }

        // Check file size from the open handle (not a separate stat).
        let file_len = file
            .metadata()
            .with_context(|| format!("statting {}", path.display()))?
            .len();
        if file_len > MAX_READ_BYTES {
            bail!(
                "{} is {} bytes, over this tool's {MAX_READ_BYTES}-byte cap — it's too large to \
                 load whole; use `grep` to search it or `bash` (`sed`/`head`/`tail`) to slice out \
                 the range you need",
                path.display(),
                file_len
            );
        }

        // Read from the already-opened handle.
        let mut text = String::new();
        match file.read_to_string(&mut text) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                bail!(
                    "{} is not a text file (invalid UTF-8) — this tool only reads text; \
                     inspect binaries via bash (`file`, `hexdump -C`, `strings`) if needed",
                    path.display()
                );
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        }
        let total_lines = text.lines().count();
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
        let start = if a.full {
            1
        } else {
            a.offset.unwrap_or(1).max(1)
        };
        let limit = if a.full {
            total_lines
        } else {
            a.limit.unwrap_or(DEFAULT_READ_LIMIT)
        };
        let mut out = String::new();
        let mut any_line_truncated = false;
        // The last line number actually emitted (0 = none) and whether the read
        // stopped at the budget rather than EOF/limit — drives the coverage record
        // and the "more to read" hint.
        let mut last_line = start.saturating_sub(1);
        let mut budget_stopped = false;
        for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
            let n = i + 1;
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
}

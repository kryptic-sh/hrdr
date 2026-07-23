use std::io::Read;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

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
    /// Read the whole file WITHOUT clipping long lines (and ignoring
    /// `offset`/`limit`), so a file with a line over `MAX_LINE` bytes can still be
    /// marked fully read — the escape hatch for a full rewrite via `write`. Costs
    /// tokens (nothing is clipped), so it is opt-in; the 50 MB file cap still
    /// applies, and if the unclipped content overflows the output budget the read
    /// is still recorded as partial.
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
         page through large files. A read that doesn't cover the whole file — `offset`/`limit` \
         short of EOF, or any line over 2000 bytes (clipped) — marks the file partially-read; \
         `edit` still works against it, but `write` refuses to overwrite a file that \
         hasn't been read in full. You must read a file before editing it. To rewrite a file \
         that has a line over 2000 bytes — which a normal read clips every time, so it can \
         never be marked fully read — read it once with `full: true` (whole file, no clipping) \
         first."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to cwd."},
                "offset": {"type": "integer", "description": "1-based line to start at (default 1)."},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)."},
                "full": {"type": "boolean", "description": "Read the entire file with NO line clipping (ignores offset/limit). Use this to fully read a file that has a very long line so a subsequent `write` rewrite is accepted. Costs more tokens; the 50 MB file cap still applies. Default false."}
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
        // `full` reads the whole file with no per-line clip (ignoring
        // offset/limit), so a file with a line over `MAX_LINE` can still be read
        // in full and marked complete — the legitimate path to a `write` rewrite.
        // Otherwise page with offset/limit and clip over-long lines for economy.
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
            out.push_str(&format!("{n:>6}: {}\n", &line[..cut]));
        }
        if out.is_empty() {
            out.push_str("(file is empty or offset past end)");
        }
        // The read covered the whole file only if it started at line 1, its
        // window reached EOF, no line was clipped to `MAX_LINE`, and the output
        // wasn't byte-truncated below. A partial read is recorded as such so a
        // later `write` (full overwrite) is refused rather than dropping the
        // unseen remainder.
        //
        // Note: a file with any line over `MAX_LINE` is *permanently* partial
        // — no offset/limit combination ever sees that line whole, so this
        // never flips to `complete` no matter how many times it's re-read.
        // `write`'s refusal message says as much and points at `edit`/`bash`
        // instead, so the model doesn't loop on read-then-write retries that
        // can never succeed.
        let byte_truncated = out.len() > ctx.max_output;
        let complete = start == 1
            && start - 1 + limit >= total_lines
            && !any_line_truncated
            && !byte_truncated;
        if complete {
            ctx.mark_read(&path);
        } else {
            ctx.mark_read_partial(&path);
        }
        Ok(truncate(&out, ctx.max_output))
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

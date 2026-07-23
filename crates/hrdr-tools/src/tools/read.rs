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
         hasn't been read in full. You must read a file before editing it."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to cwd."},
                "offset": {"type": "integer", "description": "1-based line to start at (default 1)."},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)."}
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
        let start = a.offset.unwrap_or(1).max(1);
        let limit = a.limit.unwrap_or(DEFAULT_READ_LIMIT);
        let total_lines = text.lines().count();
        let mut out = String::new();
        let mut any_line_truncated = false;
        for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
            let n = i + 1;
            let cut = crate::floor_char_boundary(line, MAX_LINE);
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

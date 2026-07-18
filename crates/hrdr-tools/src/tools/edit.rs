use std::borrow::Cow;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

use super::MAX_READ_BYTES;
use super::mutation::apply_file_change;
use super::write::unified_diff;

/// Ceiling on the projected output of a `replace_all`. A growing replacement
/// (`old="e"`, `new=50KB`) across even a modest file can project to gigabytes —
/// enough to OOM the process before the `String` finishes allocating. 64 MiB is
/// far above any legitimate edit, so this only ever trips pathological input.
pub(crate) const MAX_EDIT_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// True if `text`'s newline convention is CRLF-dominant: it has at least one
/// `\r\n` pair, and at least as many of those as bare (non-`\r`-preceded)
/// `\n`s. `read`'s `str::lines()` strips `\r`, so a model reading a CRLF file
/// only ever sees `\n`-separated lines and copies `old_string` accordingly —
/// this lets `edit` recover the match instead of failing forever. Files with
/// no CRLF at all (`crlf == 0`) are never treated as CRLF, and a file that's
/// mostly LF with a few stray `\r\n`s is left to the exact-match path as-is,
/// so a minority CRLF region can't be corrupted by a wholesale translation.
fn is_crlf_dominant(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf_only = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf_only += 1;
            }
        }
    }
    crlf > 0 && crlf >= lf_only
}

/// Translate bare `\n` to `\r\n`, leaving any `\n` already preceded by `\r`
/// untouched — so a `\r\n` already present in the input is never doubled into
/// `\r\r\n`.
fn lf_to_crlf(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, ch) in s.char_indices() {
        if ch == '\n' && !(i > 0 && bytes[i - 1] == b'\r') {
            out.push('\r');
        }
        out.push(ch);
    }
    out
}

// ---- edit ----

pub struct EditTool;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Replace an exact substring in a file (the preferred, token-cheap way to change \
         it). Copy `old_string` exactly from read output — same whitespace, line-number \
         prefixes stripped — and include enough surrounding lines to be unique. Requires \
         having read the file first. For a project-wide substitution, use `replace`; \
         prefer `edit` for a single small change."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit, absolute or relative to cwd."},
                "old_string": {"type": "string", "description": "Exact text to replace (include surrounding context to make it unique)."},
                "new_string": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: EditArgs = crate::tool_args("edit", args)?;
        if a.old_string.is_empty() {
            bail!(
                "`old_string` is empty — that matches at every position in the file, and with \
                 `replace_all` would corrupt it; pass the exact text to replace"
            );
        }
        let path = ctx.resolve(&a.path);
        // `edit` matches `old_string` against the file's live on-disk content, so
        // a partial read is fine — but the model must have read it at all, and its
        // view must not be stale (a change on disk since could move or erase the
        // text it's matching).
        match ctx.read_state(&path) {
            crate::ReadState::Unread => bail!(
                "you haven't read {} yet — call read first, then copy old_string \
                 exactly from its output",
                path.display()
            ),
            crate::ReadState::Stale => bail!(
                "{} changed on disk since you read it — re-read it and copy old_string \
                 from the current content",
                path.display()
            ),
            crate::ReadState::Partial | crate::ReadState::Fresh => {}
        }
        // Stat before reading: `read_to_string` buffers the whole file, so a
        // multi-gigabyte target would OOM before a single match is found. Reuse
        // `read`'s cap — an edit to a file larger than `read` can even show is a
        // mistake, not a workflow to support.
        if let Ok(meta) = tokio::fs::metadata(&path).await
            && meta.len() > MAX_READ_BYTES
        {
            bail!(
                "{} is {} bytes; too large to edit — narrow the change or use `replace`/`bash`",
                path.display(),
                meta.len()
            );
        }
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let mut old_string: Cow<str> = Cow::Borrowed(&a.old_string);
        let mut new_string: Cow<str> = Cow::Borrowed(&a.new_string);
        let mut count = text.matches(old_string.as_ref()).count();
        if count == 0
            && a.old_string.contains('\n')
            && !a.old_string.contains("\r\n")
            && is_crlf_dominant(&text)
        {
            // `read` renders lines via `str::lines()`, which strips `\r` — so a
            // model reading a CRLF file only ever sees `\n`-separated content
            // and copies `old_string` with bare `\n`s. Retry the match against
            // a CRLF-translated form before giving up, so a CRLF checkout
            // doesn't turn every multi-line edit into an infinite retry loop.
            let translated_old = lf_to_crlf(&a.old_string);
            let translated_count = text.matches(translated_old.as_str()).count();
            if translated_count > 0 {
                old_string = Cow::Owned(translated_old);
                new_string = Cow::Owned(lf_to_crlf(&a.new_string));
                count = translated_count;
            }
        }
        if count == 0 {
            // The #1 retry cause: right text, wrong whitespace. Detect it and
            // say so instead of the generic error.
            let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
            let normalized_old = norm(&a.old_string);
            if !normalized_old.is_empty() && norm(&text).contains(&normalized_old) {
                bail!(
                    "old_string not found in {}, but a near-match differing only in \
                     whitespace/indentation exists — copy the exact text from read \
                     output (keep tabs/spaces, strip the line-number prefix)",
                    path.display()
                );
            }
            bail!(
                "old_string not found in {} — the file may have changed since you read it; \
                 re-read it and copy the exact current text (whitespace included, no \
                 line-number prefixes)",
                path.display()
            );
        }
        if count > 1 && !a.replace_all {
            bail!(
                "old_string is not unique in {} ({count} matches) — include more \
                 surrounding lines to pin one occurrence, or set replace_all",
                path.display()
            );
        }
        let updated = if a.replace_all {
            // Bound the allocation before making it: only a growing replacement
            // can blow up, and its output size is exactly computable from the
            // match count. Bail rather than let `String::replace` OOM.
            if new_string.len() > old_string.len() {
                let projected = text
                    .len()
                    .saturating_add(count.saturating_mul(new_string.len() - old_string.len()));
                if projected > MAX_EDIT_OUTPUT_BYTES {
                    bail!(
                        "this edit would produce ~{projected} bytes; narrow `old_string` or \
                         drop `replace_all`"
                    );
                }
            }
            text.replace(old_string.as_ref(), new_string.as_ref())
        } else {
            text.replacen(old_string.as_ref(), new_string.as_ref(), 1)
        };
        let fc = apply_file_change(ctx, &path, "edit", &updated).await?;
        // Re-record with the post-edit (post-hook) signature, so a follow-up
        // edit/write this turn sees Fresh rather than a false Stale.
        ctx.mark_read(&path);
        let mut warn = fc.notes.join("\n");
        if !warn.is_empty() {
            warn.insert(0, '\n');
        }
        let diff = unified_diff(&path.display().to_string(), &text, &fc.content_after);
        Ok(truncate(
            &format!(
                "Replaced {count} occurrence(s) in {}{warn}\n{diff}",
                path.display()
            ),
            ctx.max_output,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;

    /// A file over `read`'s size cap is refused before `read_to_string` would
    /// buffer it whole, and the byte count is in the message so the model knows
    /// why. A sparse file (`set_len`) hits the cap without writing 50+ MiB.
    #[tokio::test]
    async fn edit_refuses_a_file_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.txt");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_READ_BYTES + 1).unwrap();
        drop(f);
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let err = EditTool
            .execute(
                json!({"path": path.to_str().unwrap(), "old_string": "a", "new_string": "b"}),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("too large to edit"), "{err}");
        assert!(
            err.contains(&(MAX_READ_BYTES + 1).to_string()),
            "the byte count must be reported: {err}"
        );
    }

    /// A `replace_all` whose projected output blows past the expansion cap is
    /// refused *before* the giant `String` is allocated — the guard is
    /// arithmetic on the match count, not a failed allocation.
    #[tokio::test]
    async fn edit_refuses_a_replace_all_that_would_explode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        // 2000 "e"s → 2000 matches; each grows by ~50 KB → ~100 MB projected,
        // well over the 64 MiB cap, but the file and replacement are tiny.
        std::fs::write(&path, "e".repeat(2000)).unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let big = "x".repeat(50_000);
        let err = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "e",
                    "new_string": big,
                    "replace_all": true,
                }),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("would produce"), "{err}");
        assert!(err.contains("narrow"), "{err}");
        // The file is untouched — the guard fired before any write.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "e".repeat(2000));
    }

    /// `read` strips `\r` via `str::lines()`, so a model reading a CRLF file
    /// copies `old_string` with bare `\n`s. A multi-line edit with such an
    /// `old_string` must still succeed against the real `\r\n` file, and the
    /// file must keep its CRLF endings afterward — including in the untouched
    /// lines, and in the newly written region.
    #[tokio::test]
    async fn edit_matches_lf_old_string_against_crlf_file_and_keeps_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        std::fs::write(&path, "line1\r\nline2\r\nline3\r\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "line1\nline2\n",
                    "new_string": "replaced1\nreplaced2\n",
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");

        let bytes = std::fs::read(&path).unwrap();
        let on_disk = String::from_utf8(bytes).unwrap();
        assert_eq!(on_disk, "replaced1\r\nreplaced2\r\nline3\r\n");
        assert!(
            on_disk.contains("\r\n"),
            "the file must keep CRLF endings: {on_disk:?}"
        );
    }

    /// An LF file is completely unaffected by the CRLF-recovery path: no
    /// `\r\n` is ever introduced.
    #[tokio::test]
    async fn edit_lf_file_is_unaffected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lf.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "line1\nline2\n",
                    "new_string": "replaced1\nreplaced2\n",
                }),
                &c,
            )
            .await
            .unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "replaced1\nreplaced2\nline3\n");
        assert!(
            !on_disk.contains('\r'),
            "an LF file must never gain CR bytes: {on_disk:?}"
        );
    }

    /// `replace_all` on a CRLF file matches every occurrence via the
    /// CRLF-translated `old_string`, and every replacement keeps `\r\n`.
    #[tokio::test]
    async fn edit_replace_all_across_crlf_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf_all.txt");
        std::fs::write(&path, "foo: 1\r\nbar\r\nfoo: 2\r\nbar\r\nfoo: 3\r\nbar\r\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "bar\n",
                    "new_string": "baz\n",
                    "replace_all": true,
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 3 occurrence"), "{out}");

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            on_disk,
            "foo: 1\r\nbaz\r\nfoo: 2\r\nbaz\r\nfoo: 3\r\nbaz\r\n"
        );
    }

    /// A single-line `old_string` (no `\n`) on a CRLF file already matches
    /// literally — no translation is needed, and the fix must not disturb
    /// that existing path.
    #[tokio::test]
    async fn edit_single_line_old_string_on_crlf_file_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf_single.txt");
        std::fs::write(&path, "line1\r\nline2\r\nline3\r\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "line2",
                    "new_string": "replaced2",
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "line1\r\nreplaced2\r\nline3\r\n");
    }

    /// A multi-line `old_string` whose region doesn't exist in either LF or the
    /// CRLF-translated form fails safe: a clean "not found" error, the file left
    /// byte-for-byte untouched — never a partial or corrupting edit.
    #[tokio::test]
    async fn edit_that_matches_in_neither_form_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf_nomatch.txt");
        let original = "alpha\r\nbeta\r\ngamma\r\n";
        std::fs::write(&path, original).unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let err = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    // Real lines, but never adjacent — no such region exists.
                    "old_string": "alpha\ngamma",
                    "new_string": "x",
                }),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        // The bytes on disk are exactly what they were.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

use std::borrow::Cow;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext};

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

/// " — modified by `cargo fmt --all`" when we know which shell command changed
/// the file, empty when we don't (the user's editor, a background process).
fn culprit_clause(ctx: &ToolContext, path: &std::path::Path) -> String {
    match ctx.change_culprit(path) {
        Some(cmd) => format!(" — modified by `{cmd}`"),
        None => String::new(),
    }
}

/// The refusal for an edit whose file changed on disk since the read *and* whose
/// anchor no longer pins one spot in it. Names the command that changed the file
/// when known: an unexplained "changed on disk" reads like a bug in our
/// bookkeeping, and the observed reaction is to stop using `edit` at all,
/// whereas a named formatter points straight at "re-read and retry".
fn stale_error(ctx: &ToolContext, path: &std::path::Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{} changed on disk since you read it{} — re-read it and copy old_string \
         from the current content",
        path.display(),
        culprit_clause(ctx, path)
    )
}

/// Per-line normalization for the fuzzy retry: trailing whitespace trimmed and
/// typographic variants mapped 1:1 (smart quotes → ASCII, dashes → hyphen,
/// figure/NBSP spaces → space). Deliberately no NFKC and no internal space
/// collapsing: every normalized char maps back to exactly one original char
/// (before the trimmed tail), so a match in normalized space recovers its
/// original byte span. Line boundaries are untouched, so lines correspond 1:1.
fn fuzzy_norm_line(line: &str) -> String {
    line.trim_end_matches(char::is_whitespace)
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' => ' ',
            _ => c,
        })
        .collect()
}

/// One file line prepared for the fuzzy matcher: byte offsets plus the
/// normalized form and, per normalized char, the byte offset of its original
/// char (normalization is 1:1 over the trimmed prefix, so char `c` of `norm`
/// is char `c` of the line's trimmed content).
struct FuzzyLine {
    /// Byte offset of the line content (after the preceding newline).
    start: usize,
    /// Byte offset one past the content (before any newline).
    content_end: usize,
    /// Byte offset one past the content *and* its newline (== `content_end`
    /// for the final line when the text has no trailing newline).
    end_incl_nl: usize,
    norm: String,
    byte_of_char: Vec<usize>,
}

impl FuzzyLine {
    fn new(start: usize, content: &str, text_len: usize) -> Self {
        let content_end = start + content.len();
        let end_incl_nl = if content_end < text_len {
            content_end + 1
        } else {
            content_end
        };
        let trimmed = content.trim_end_matches(char::is_whitespace);
        let mut byte_of_char = Vec::with_capacity(trimmed.chars().count());
        let mut b = 0;
        for ch in trimmed.chars() {
            byte_of_char.push(b);
            b += ch.len_utf8();
        }
        let norm = fuzzy_norm_line(content);
        Self {
            start,
            content_end,
            end_incl_nl,
            norm,
            byte_of_char,
        }
    }
}

/// Find the byte spans in `text` that `old` occupies when the only differences
/// from the file are line-end whitespace and typographic characters (see
/// [`fuzzy_norm_line`]). A single-line `old` may sit anywhere within a file
/// line; a multi-line `old`'s first line must END its file line (it is
/// followed by a newline), its last line must BEGIN its, and interior lines
/// must match whole. A trailing newline in `old` extends the span through the
/// last matched line's newline. Returns all non-overlapping spans in ascending
/// order; empty when there is no fuzzy match — including when any line of
/// `old` normalizes to nothing, since blank lines would match vacuously
/// everywhere.
fn fuzzy_match_spans(text: &str, old: &str) -> Vec<(usize, usize)> {
    let mut old_lines: Vec<&str> = old.split('\n').collect();
    if old_lines.last() == Some(&"") {
        old_lines.pop(); // `old` ended with a newline: no trailing content line
    }
    if old_lines.is_empty() {
        return Vec::new();
    }
    let old_norm: Vec<String> = old_lines.iter().map(|l| fuzzy_norm_line(l)).collect();
    if old_norm.iter().any(|l| l.is_empty()) {
        return Vec::new();
    }
    let old_ends_with_nl = old.ends_with('\n');
    let k = old_norm.len();

    let mut lines: Vec<FuzzyLine> = Vec::new();
    let mut start = 0;
    for content in text.split('\n') {
        lines.push(FuzzyLine::new(start, content, text.len()));
        start += content.len() + 1;
    }

    let mut spans = Vec::new();
    if k == 1 {
        // Single-line `old`: a substring anywhere within a file line. A match
        // that reaches the line's trimmed end also consumes the trailing
        // whitespace the model never copied.
        let needle = &old_norm[0];
        for line in &lines {
            let mut search_from = 0;
            while let Some(rel) = line.norm[search_from..].find(needle) {
                // `find` yields byte offsets; the byte-of-char table is indexed
                // by char, so convert (a norm is not all-ASCII once any other
                // non-mapped char — é, CJK — appears before the match).
                let abs_byte = search_from + rel;
                let s_char = line.norm[..abs_byte].chars().count();
                let e_char = s_char + needle.chars().count();
                let span_end = if old_ends_with_nl {
                    line.end_incl_nl
                } else if e_char == line.byte_of_char.len() {
                    line.content_end
                } else {
                    line.start + line.byte_of_char[e_char]
                };
                spans.push((line.start + line.byte_of_char[s_char], span_end));
                search_from = abs_byte + needle.len();
            }
        }
    } else {
        // Multi-line `old`: first line is a suffix of its file line, last a
        // prefix, interior lines equal.
        let first = &old_norm[0];
        let last = &old_norm[k - 1];
        for i in 0..=lines.len().saturating_sub(k) {
            let tail = &lines[i..];
            let first_line = &tail[0];
            let last_line = &tail[k - 1];
            if !first_line.norm.ends_with(first) {
                continue;
            }
            if !last_line.norm.starts_with(last) {
                continue;
            }
            if tail[1..k - 1]
                .iter()
                .zip(&old_norm[1..k - 1])
                .any(|(l, o)| l.norm != *o)
            {
                continue;
            }
            let s_char = first_line.norm.len() - first.len();
            let span_start = first_line.start + first_line.byte_of_char[s_char];
            let e_char = last.len();
            let span_end = if old_ends_with_nl {
                last_line.end_incl_nl
            } else if e_char == last_line.norm.len() {
                last_line.content_end
            } else {
                last_line.start + last_line.byte_of_char[e_char]
            };
            spans.push((span_start, span_end));
        }
        // Candidates are line-ranges, so a later one can overlap an earlier
        // (a line ending in `first` that also equals an interior line) — drop
        // the overlap so `replace_all` never double-replaces a byte.
        let mut dedup: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for span in spans {
            if dedup.last().is_none_or(|&(_, e)| span.0 >= e) {
                dedup.push(span);
            }
        }
        spans = dedup;
    }
    spans
}

// ---- edit ----

pub struct EditTool;

#[derive(Deserialize)]
struct EditArgs {
    // Same path-name synonyms `read` accepts (see `ReadArgs`) — unambiguous
    // here, this tool takes exactly one path.
    #[serde(
        alias = "file_path",
        alias = "filepath",
        alias = "file",
        alias = "filename",
        alias = "file_name",
        alias = "path_to_file"
    )]
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
                "replace_all": {"type": "boolean", "default": false, "description": "Replace every occurrence (default false)."}
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
        let path = ctx.resolve_write(&a.path)?;
        if let Some(reason) = crate::secret_file_reason(&crate::canonicalize_nearest(&path)) {
            bail!(
                "refusing to edit {}: {reason} — secret/credential files are off-limits to \
                 the write/edit tools; if the user genuinely needs this, they must provide it",
                path.display()
            );
        }
        // `edit` matches `old_string` against the file's live on-disk content, so
        // a partial read is fine — but the model must have read it at all.
        //
        // A change on disk since the read does *not* sink the edit by itself: the
        // dominant cause is a formatter (`cargo fmt`, `prettier`) reflowing lines
        // the edit doesn't touch, and refusing there taught models to distrust
        // `edit` and rewrite whole files instead. The verdict is carried down to
        // the match instead — a unique match against the *current* content means
        // the anchor is live and the edit is safe (see `stale` below).
        let stale = match ctx.read_state(&path) {
            crate::ReadState::Unread => bail!(
                "you haven't read {} yet — call read first, then copy old_string \
                 exactly from its output",
                path.display()
            ),
            crate::ReadState::Stale => true,
            crate::ReadState::Partial | crate::ReadState::Fresh => false,
        };
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
        // Byte spans of the fuzzy retry (see the count == 0 arm); empty when
        // the exact or CRLF-translated match won.
        let mut fuzzy_spans: Vec<(usize, usize)> = Vec::new();
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
        // The stale-read verdict, resolved against the live content: exactly one
        // match means the model's anchor is current-file text, so apply the edit
        // (and say so in the result). Anything else — the anchor is gone, or has
        // become ambiguous — is the old refusal: the model cannot know what the
        // change did, so it must re-read rather than guess.
        if stale && count != 1 {
            return Err(stale_error(ctx, &path));
        }
        if count == 0 {
            // The fuzzy retry: `read` shows lines as-is, and the failure modes
            // for a model copying them are exactly the things read output makes
            // invisible or easy to drop — trailing line-end whitespace, and
            // typographic characters (smart quotes, dashes, non-breaking
            // spaces). Retry the match with those normalizations instead of
            // failing forever; a unique match is applied and the result says
            // the match was fuzzy, so the model sees what actually changed.
            fuzzy_spans = fuzzy_match_spans(&text, &a.old_string);
            if !fuzzy_spans.is_empty() {
                count = fuzzy_spans.len();
                if is_crlf_dominant(&text) {
                    new_string = Cow::Owned(lf_to_crlf(&a.new_string));
                }
            } else {
                // The #1 retry cause: right text, wrong whitespace. Detect it
                // and say so instead of the generic error.
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
        }
        if count > 1 && !a.replace_all {
            bail!(
                "old_string is not unique in {} ({count} matches) — include more \
                 surrounding lines to pin one occurrence, or set replace_all",
                path.display()
            );
        }
        // Read the attribution before the write: `mark_read` below clears it (the
        // file is no longer stale afterwards), so this is the last moment the
        // reason is still on record.
        let stale_note = if stale {
            format!(
                "\nnote: the file had changed on disk since your last read{}; your anchor \
                 still matched uniquely, so the edit was applied — the diff below reflects \
                 the current file",
                culprit_clause(ctx, &path)
            )
        } else {
            String::new()
        };
        // A fuzzy match is a report, not a silent success: the model must see
        // that its old_string differed from the file (trailing whitespace or
        // typographic characters) and what the diff actually changed, or it
        // keeps copying the variant that only fuzzy-matches.
        let fuzzy_note = if fuzzy_spans.is_empty() {
            String::new()
        } else {
            "\nnote: old_string matched only after normalizing line-end whitespace and \
             quote/dash characters — the diff below shows what actually changed"
                .to_string()
        };
        let updated = if !fuzzy_spans.is_empty() {
            // Bound the allocation before splicing: the output size is exactly
            // computable from the spans and the replacement length.
            let projected = text.len()
                + fuzzy_spans
                    .iter()
                    .map(|&(s, e)| new_string.len().saturating_sub(e - s))
                    .sum::<usize>();
            if projected > MAX_EDIT_OUTPUT_BYTES {
                bail!(
                    "this edit would produce ~{projected} bytes; narrow `old_string` or \
                     drop `replace_all`"
                );
            }
            let mut out = text.to_string();
            for &(s, e) in fuzzy_spans.iter().rev() {
                out.replace_range(s..e, new_string.as_ref());
            }
            out
        } else if a.replace_all {
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
        let warn = fc.formatted_notes();
        let diff = unified_diff(&path.display().to_string(), &text, &fc.content_after);
        // The full diff rides back uncapped — it is what the transcript shows
        // the user; the agent abbreviates the model's copy.
        Ok(format!(
            "Replaced {count} occurrence(s) in {}{warn}{fuzzy_note}{stale_note}\n{diff}",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;

    /// The `read` path-name synonyms work here too — a call spelled `file` (or
    /// `file_path`) must not die on a "missing field `path`".
    #[test]
    fn edit_args_accept_path_aliases() {
        for key in ["file", "file_path", "filename", "path"] {
            let a: EditArgs =
                serde_json::from_value(json!({key: "x", "old_string": "a", "new_string": "b"}))
                    .unwrap_or_else(|e| panic!("alias {key:?}: {e}"));
            assert_eq!(a.path, "x");
        }
    }

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

    /// `edit` mutates, so its path goes through the write guard — and the
    /// guard runs *before* the read-state and staleness machinery, so the
    /// model gets the boundary error rather than a confusing "read it first".
    #[tokio::test]
    async fn edit_outside_roots_is_refused() {
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim.rs");
        std::fs::write(&target, "fn old() {}\n").unwrap();

        let ctx = crate::sandbox::confined_ctx(cwd.path(), crate::SandboxMode::Write);
        let err = EditTool
            .execute(
                json!({
                    "path": target.to_str().unwrap(),
                    "old_string": "old",
                    "new_string": "new",
                }),
                &ctx,
            )
            .await
            .expect_err("an edit outside the roots must be refused")
            .to_string();
        assert!(err.contains("sandbox: refusing to write"), "{err}");
        assert!(err.contains("You may write only under"), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fn old() {}\n");
    }

    /// The model's `old_string` can differ from the file in trailing line-end
    /// whitespace (invisible in `read` output and easy to drop when copying).
    /// The fuzzy retry recovers such a match, consumes the trailing whitespace,
    /// and the result says the match was fuzzy.
    #[tokio::test]
    async fn edit_recovers_a_match_with_trailing_line_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn old() {  \n    let x = 1;\n}\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "fn old() {\n    let x = 1;\n}\n",
                    "new_string": "fn new() {\n    let x = 1;\n}\n",
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");
        assert!(
            out.contains("normalizing line-end whitespace"),
            "a fuzzy match must be reported: {out}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn new() {\n    let x = 1;\n}\n",
            "the trailing whitespace the model never copied is consumed"
        );
    }

    /// Smart quotes and dashes (the typography formatters apply) normalize to
    /// their ASCII forms, so an edit whose only difference is those characters
    /// succeeds — and replaces exactly the quoted span, leaving the rest of the
    /// line untouched.
    #[tokio::test]
    async fn edit_recovers_typographic_quote_and_dash_variants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.md");
        std::fs::write(&path, "let s = \u{201C}hello\u{201D}; // a \u{2014} b\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "\"hello\"",
                    "new_string": "\"world\"",
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "let s = \"world\"; // a \u{2014} b\n",
            "only the quoted span is replaced; the dash outside the span stays"
        );

        // A dash difference is covered too, when the old_string spans it.
        let path = dir.path().join("g.md");
        std::fs::write(&path, "// a \u{2014} b\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);
        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "// a - b",
                    "new_string": "// a - c",
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "// a - c\n",
            "the em dash is consumed with the matched line"
        );
    }

    /// A fuzzy match after a multi-byte character must land on the right
    /// bytes: `find` yields byte offsets while the recovery table is
    /// char-indexed, so a non-ASCII char before the match used to shift the
    /// span and corrupt the edit.
    #[tokio::test]
    async fn edit_fuzzy_match_after_a_multibyte_char_lands_on_the_right_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "café \u{201C}bon\u{201D}!\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "\"bon\"",
                    "new_string": "\"très\"",
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "café \"très\"!\n",
            "the smart-quoted span is replaced exactly, bytes after the é intact"
        );
    }

    /// The fuzzy retry on a CRLF file keeps CRLF endings and translates the
    /// replacement the same way the exact CRLF path does.
    #[tokio::test]
    async fn edit_fuzzy_on_crlf_keeps_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf_fuzzy.txt");
        std::fs::write(&path, "alpha \r\nbeta\r\ngamma\r\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "alpha\nbeta\n",
                    "new_string": "replaced\n",
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence"), "{out}");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "replaced\r\ngamma\r\n");
        assert!(
            on_disk.contains("\r\n"),
            "CRLF endings are kept: {on_disk:?}"
        );
    }

    /// A fuzzy match that is not unique refuses exactly like the exact path:
    /// same "not unique" message, same untouched file. (`old` ends in a
    /// newline so it does not match the lines exactly — the trailing spaces
    /// are what the fuzzy path bridges.)
    #[tokio::test]
    async fn edit_fuzzy_requires_a_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "foo \nfoo  \n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let err = EditTool
            .execute(
                json!({"path": path.to_str().unwrap(), "old_string": "foo\n", "new_string": "bar\n"}),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not unique"), "{err}");
        assert!(err.contains("2 matches"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo \nfoo  \n");
    }

    /// `replace_all` applies every fuzzy match, exactly like the exact path.
    #[tokio::test]
    async fn edit_fuzzy_replace_all_replaces_every_fuzzy_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "foo \nfoo  \n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let out = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "foo\n",
                    "new_string": "bar\n",
                    "replace_all": true,
                }),
                &c,
            )
            .await
            .unwrap();
        assert!(out.contains("Replaced 2 occurrence"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "bar\nbar\n",
            "both fuzzy occurrences, trailing whitespace and newline consumed"
        );
    }

    /// When even the normalized form does not match, the edit still refuses
    /// with the informative message and leaves the file byte-for-byte intact.
    #[tokio::test]
    async fn edit_with_no_fuzzy_match_still_refuses_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let err = EditTool
            .execute(
                json!({"path": path.to_str().unwrap(), "old_string": "gamma", "new_string": "x"}),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nbeta\n");
    }

    /// An `old_string` with blank lines normalizes to an empty line, which
    /// would match every file line vacuously — the fuzzy path refuses rather
    /// than guess, and the file is untouched.
    #[tokio::test]
    async fn edit_fuzzy_refuses_when_old_has_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "alpha\n\nbeta\n").unwrap();
        let c = ToolContext::new(dir.path());
        c.mark_read(&path);

        let err = EditTool
            .execute(
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "alpha\n\nx",
                    "new_string": "y",
                }),
                &c,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\n\nbeta\n");
    }
}

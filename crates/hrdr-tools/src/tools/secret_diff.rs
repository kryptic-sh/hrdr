//! Redaction of secret file contents out of a git diff.
//!
//! `git diff`/`show`/`log -p` will happily print the body of a `.env`, an
//! `id_rsa` or an `~/.aws/credentials` that a commit touched. The read tools
//! refuse those files outright ([`crate::secret_file_reason`]); a diff is the
//! back door, and the only one left now that the bespoke read-only `git` tool
//! is gone and git runs through the shell.
//!
//! So this would apply where hrdr composes a diff ITSELF and hands it to a
//! model. Nothing does today — see the note on the function. It is not a
//! guarantee about arbitrary shell output: a model that runs `git diff` in the
//! shell gets what git prints, exactly as `cat` would. The shell has never been
//! a redaction boundary, and pretending otherwise would be worse than the
//! honest limit.

/// The file path a diff-section header names, if `line` starts one:
/// `diff --git a/<p> b/<p>` (prefer the `b/` destination), or a merge diff's
/// `diff --cc <p>` / `diff --combined <p>`. `None` for any other line.
///
/// Under the default `core.quotePath`, git C-style-quotes a path that has a
/// space, a double quote, a backslash, or a non-ASCII byte —
/// `diff --git "a/my dir/.env" "b/my dir/.env"` — so this can't just scan for
/// literal `" b/"`; it tokenizes the two (possibly quoted) paths and unquotes
/// whichever one is quoted.
///
/// `--no-prefix`/`--src-prefix`/`--dst-prefix` would otherwise strip the
/// `a/`/`b/` markers this still relies on to tell the two tokens apart.
pub(crate) fn diff_section_path(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        if let Some((_a, remainder)) = take_diff_header_token(rest)
            && let Some((b, _)) = take_diff_header_token(remainder)
        {
            // The token is the whole `b/<path>` destination spelling
            // (unquoted) — strip the `b/` marker to get the bare path, same
            // as the old `" b/"`-scan fallback below did.
            return Some(b.strip_prefix("b/").map(str::to_string).unwrap_or(b));
        }
        // Fall back to the old best-effort scan for a header this tokenizer
        // can't make sense of, rather than silently losing the path.
        if let Some(idx) = rest.rfind(" b/") {
            return Some(rest[idx + 3..].to_string());
        }
        return rest
            .strip_prefix("a/")
            .map(|p| p.split(' ').next().unwrap_or(p).to_string());
    }
    for pre in ["diff --cc ", "diff --combined "] {
        if let Some(rest) = line.strip_prefix(pre) {
            return Some(unquote_c_style(rest));
        }
    }
    None
}

/// What replaces a withheld hunk. Shared with `shell`'s streaming redaction so
/// the two paths cannot describe the same thing differently.
pub(crate) const REDACTED_DIFF_MARKER: &str =
    "[redacted: this file is a credential/secret store — its diff is withheld]";

/// Consume one whitespace-delimited `diff --git` header token from the start
/// of `s`, which may be a bare path (`a/foo`) or a C-style-quoted one
/// (`"a/my dir/.env"`), returning the unquoted token and whatever follows the
/// single separating space. `None` if `s` is empty or a quoted token's closing
/// quote is missing (malformed input — let the caller fall back).
fn take_diff_header_token(s: &str) -> Option<(String, &str)> {
    if let Some(inner) = s.strip_prefix('"') {
        let bytes = inner.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if i + 1 < bytes.len() => i += 2,
                b'"' => {
                    let token = &inner[..i];
                    let remainder = inner[i + 1..].strip_prefix(' ').unwrap_or(&inner[i + 1..]);
                    return Some((unquote_c_style(&format!("\"{token}\"")), remainder));
                }
                _ => i += 1,
            }
        }
        None
    } else if s.is_empty() {
        None
    } else {
        let (token, remainder) = s.split_once(' ').unwrap_or((s, ""));
        Some((token.to_string(), remainder))
    }
}

/// Unquote a C-style quoted string as git emits under `core.quotePath`
/// (default on): a double-quoted token where `\\`, `\"`, `\t`, `\n`, `\r`, and
/// `\NNN` (octal byte — how a non-ASCII UTF-8 byte is spelled) stand for the
/// literal byte. Returns `s` unchanged if it isn't a quoted token — git only
/// quotes a path that needs it.
fn unquote_c_style(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_string();
    };
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'"' => {
                    out.push(b'"');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b'a' => {
                    out.push(0x07);
                    i += 2;
                }
                b'b' => {
                    out.push(0x08);
                    i += 2;
                }
                b'f' => {
                    out.push(0x0c);
                    i += 2;
                }
                b'v' => {
                    out.push(0x0b);
                    i += 2;
                }
                d1 @ b'0'..=b'7'
                    if i + 3 < bytes.len()
                        && (b'0'..=b'7').contains(&bytes[i + 2])
                        && (b'0'..=b'7').contains(&bytes[i + 3]) =>
                {
                    // Widen to u32 before combining digits: a byte's worth of
                    // octal digits (each 0-7) can sum past 255 for malformed
                    // input, which would overflow a `u8` multiply/add.
                    let d1 = u32::from(d1 - b'0');
                    let d2 = u32::from(bytes[i + 2] - b'0');
                    let d3 = u32::from(bytes[i + 3] - b'0');
                    out.push((d1 * 64 + d2 * 8 + d3) as u8);
                    i += 4;
                }
                other => {
                    // Unrecognised escape: keep both characters verbatim
                    // rather than dropping the backslash.
                    out.push(b'\\');
                    out.push(other);
                    i += 2;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The streaming half of [`redact_secret_diffs`]: the same state machine, fed one
/// line at a time.
///
/// `shell` ingests output as the command runs, so it never holds a whole diff to
/// pass to the batch version — and a `git diff` that touches `.env` names the file
/// once in a header and then prints its contents as `+SECRET=…` lines that name
/// nothing at all. A per-line path filter cannot see those; this can.
///
/// A struct rather than two loose `bool`s because the caller is a macro expanded at
/// several sites, and the last expansion made a plain assignment look dead.
#[derive(Debug, Default)]
pub(crate) struct DiffRedactor {
    /// Inside the hunk body of a section for a credential file.
    redacting: bool,
    /// This section's marker has been emitted, so it is not repeated per line.
    marked: bool,
}

/// What the caller should do with a line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LineAction {
    /// Emit it unchanged.
    Keep,
    /// Drop it, and emit [`REDACTED_DIFF_MARKER`] in its place.
    ReplaceWithMarker,
    /// Drop it silently — the marker for this section is already out.
    Drop,
}

impl DiffRedactor {
    /// Classify `line`, updating the section state. `cwd` anchors the relative path
    /// a diff header carries.
    pub(crate) fn observe(&mut self, line: &str, cwd: &std::path::Path) -> LineAction {
        if let Some(path) = diff_section_path(line) {
            // A header both closes the previous section and opens the next, and is
            // always kept: the model should see THAT a credential file changed.
            self.redacting =
                crate::secret_file_reason(&crate::canonicalize_nearest(&cwd.join(path))).is_some();
            self.marked = false;
            return LineAction::Keep;
        }
        if !self.redacting {
            return LineAction::Keep;
        }
        if self.marked {
            LineAction::Drop
        } else {
            self.marked = true;
            LineAction::ReplaceWithMarker
        }
    }
}

/// Redact the hunk body of any diff section whose file is a credential/secret
/// store, keeping the section header so the model still sees *that* the file
/// changed — just not its content. Covers `diff`, `show`, and `log -p` output;
/// a no-op on plain `status`/`log`/`branch` output (no diff headers).
///
/// The shape a line-oriented path filter misses: a `git diff` that touches
/// `.env` names the file once in a header the filter would let through, and then
/// prints its contents as `+SECRET=…` lines that name nothing at all.
///
/// `pub` (re-exported from the crate root) for a caller that has a whole diff in
/// hand. `shell` cannot use it — it ingests one line at a time as the command
/// streams — so it runs the same state machine incrementally over
/// [`diff_section_path`]; the two must stay in step, and
/// `a_secret_diff_is_redacted_line_by_line_too` is what keeps them there.
pub fn redact_secret_diffs(output: &str) -> String {
    let mut out = String::with_capacity(output.len());
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(path) = diff_section_path(line) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        out.push_str(line);
        out.push('\n');
        if crate::secret_file_reason(std::path::Path::new(&path)).is_some() {
            out.push_str(REDACTED_DIFF_MARKER);
            out.push('\n');
            // Drop the rest of this section (up to the next `diff` header / EOF).
            while let Some(peek) = lines.peek() {
                if diff_section_path(peek).is_some() {
                    break;
                }
                lines.next();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A `git diff` that touches a credential file must not print its contents**,
    /// and the streaming path has to reach the same answer as the batch one.
    ///
    /// The two exist because `shell` never holds a whole diff — it ingests lines as
    /// the command runs — and a state machine duplicated in two places is one that
    /// drifts. This is what keeps them in step.
    ///
    /// It is also the leak a per-line PATH filter cannot catch: the file is named
    /// once, in a header, and its contents arrive as `+TOKEN=…` lines naming nothing.
    #[test]
    fn a_secret_diff_is_redacted_line_by_line_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "TOKEN=live\n").unwrap();
        std::fs::write(dir.path().join("src.rs"), "fn main() {}\n").unwrap();
        let diff = "\
diff --git a/.env b/.env
index 111..222 100644
--- a/.env
+++ b/.env
@@ -1 +1 @@
-TOKEN=old
+TOKEN=live
diff --git a/src.rs b/src.rs
index 333..444 100644
--- a/src.rs
+++ b/src.rs
@@ -1 +1 @@
-fn main() {}
+fn main() { work() }
";

        // Streamed one line at a time, the way `shell` sees it.
        let mut redactor = DiffRedactor::default();
        let mut streamed = String::new();
        for line in diff.lines() {
            match redactor.observe(line, dir.path()) {
                LineAction::Keep => {
                    streamed.push_str(line);
                    streamed.push('\n');
                }
                LineAction::ReplaceWithMarker => {
                    streamed.push_str(REDACTED_DIFF_MARKER);
                    streamed.push('\n');
                }
                LineAction::Drop => {}
            }
        }

        for out in [&streamed, &redact_secret_diffs(diff)] {
            // The secret is gone, and its `-`/`+` lines with it.
            assert!(!out.contains("TOKEN=live"), "{out}");
            assert!(!out.contains("TOKEN=old"), "{out}");
            // …but the model still learns THAT the file changed, and why it is blank.
            assert!(out.contains("diff --git a/.env b/.env"), "{out}");
            assert!(out.contains(REDACTED_DIFF_MARKER), "{out}");
            // Once per section, not once per dropped line.
            assert_eq!(out.matches(REDACTED_DIFF_MARKER).count(), 1, "{out}");
            // And the section AFTER it survives: a redaction must not swallow the
            // rest of the diff.
            assert!(out.contains("+fn main() { work() }"), "{out}");
        }
    }
}

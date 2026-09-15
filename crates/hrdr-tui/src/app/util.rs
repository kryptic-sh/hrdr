//! Free helper functions with no `App` receiver.

/// Split a `$VISUAL`/`$EDITOR` value into words, by this platform's convention
/// for a backslash: an escape on unix, a path separator on Windows — where
/// `EDITOR=C:\tools\vim.exe` is the value people actually set. See
/// [`split_words`].
pub(crate) fn split_shell_words(input: &str) -> Vec<String> {
    split_words(input, !cfg!(windows))
}

/// Split a command string into words using POSIX-ish shell rules.
///
/// Handles the quoting seen in `$EDITOR`/`$VISUAL` values without shelling
/// out to a real shell (which would add injection/quoting hazards):
/// - whitespace separates words;
/// - double quotes group text; a backslash inside them escapes `"` (and, with
///   `backslash_escapes`, `\`);
/// - single quotes group text literally (no escapes recognized inside);
/// - with `backslash_escapes`, a backslash outside quotes escapes the next
///   character verbatim; without it, a backslash is an ordinary character.
///
/// Unterminated quote handling: if the string ends while still inside a
/// quote (or right after a trailing backslash), the accumulated text is
/// emitted as-is rather than erroring. `$EDITOR` is trusted local config,
/// so best-effort recovery beats failing to launch the editor.
///
/// `cmd`-style `%VAR%` expansion and caret (`^`) quoting are out of scope.
fn split_words(input: &str, backslash_escapes: bool) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has_word = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut cur));
                    has_word = false;
                }
            }
            '\'' => {
                has_word = true;
                // Single quotes: everything is literal until the next quote.
                for sc in chars.by_ref() {
                    if sc == '\'' {
                        break;
                    }
                    cur.push(sc);
                }
            }
            '"' => {
                has_word = true;
                while let Some(dc) = chars.next() {
                    match dc {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some('"') => cur.push(chars.next().unwrap()),
                            Some('\\') if backslash_escapes => cur.push(chars.next().unwrap()),
                            _ => cur.push('\\'),
                        },
                        _ => cur.push(dc),
                    }
                }
            }
            '\\' if backslash_escapes => {
                has_word = true;
                // Outside quotes a backslash escapes the next char verbatim;
                // a trailing backslash is emitted literally.
                match chars.next() {
                    Some(next) => cur.push(next),
                    None => cur.push('\\'),
                }
            }
            _ => {
                has_word = true;
                cur.push(c);
            }
        }
    }

    if has_word {
        words.push(cur);
    }
    words
}

/// The editor used when neither `$VISUAL` nor `$EDITOR` names one: the one every
/// install of the platform has.
const DEFAULT_EDITOR: &str = if cfg!(windows) { "notepad" } else { "vi" };

/// Run `$VISUAL`/`$EDITOR` (falling back to [`DEFAULT_EDITOR`]) on `path`,
/// inheriting stdio. The command string may carry quoted args and paths with
/// spaces (e.g. `code --profile "Work Profile" -w`); it is parsed with
/// [`split_shell_words`], and the program is found through `PATH` the way
/// [`hrdr_tools::resolve_program`] finds every other one — VS Code's `code` is
/// `code.cmd` on Windows.
pub(crate) fn run_editor(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| DEFAULT_EDITOR.to_string());
    let mut parts = split_shell_words(&editor);
    // Empty/whitespace-only value parses to no words; fall back to the default.
    let program = if parts.is_empty() {
        DEFAULT_EDITOR.to_string()
    } else {
        parts.remove(0)
    };
    std::process::Command::new(hrdr_tools::resolve_program(&program, None))
        .args(parts)
        .arg(path)
        .status()
        .map_err(|e| std::io::Error::new(e.kind(), format!("launching `{program}`: {e}")))
}

/// The draft an editor saved, as the input box should hold it: CRLF line endings
/// (Notepad's, or any editor configured for them) folded to `\n`, and the one
/// trailing newline editors append dropped so the draft doesn't submit blank.
pub(crate) fn draft_from_editor(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    match text.strip_suffix('\n') {
        Some(trimmed) => trimmed.to_string(),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{draft_from_editor, split_words};

    /// The unix convention, which every test below exercises unless it says
    /// otherwise.
    fn split_shell_words(input: &str) -> Vec<String> {
        split_words(input, true)
    }

    /// Windows: a backslash is a path separator, not an escape, so the value
    /// people set there survives whole — quoted or not.
    #[test]
    fn windows_paths_keep_their_backslashes() {
        assert_eq!(
            split_words(r"C:\tools\vim.exe -f", false),
            [r"C:\tools\vim.exe", "-f"]
        );
        assert_eq!(
            split_words(r#""C:\Program Files\Neovim\bin\nvim.exe" --clean"#, false),
            [r"C:\Program Files\Neovim\bin\nvim.exe", "--clean"]
        );
        // An escaped quote inside double quotes is still a quote.
        assert_eq!(split_words(r#""a\"b""#, false), [r#"a"b"#]);
    }

    /// What an editor saved comes back as the draft: CRLF folded, one trailing
    /// newline dropped, anything else kept.
    #[test]
    fn a_saved_draft_folds_crlf_and_drops_one_trailing_newline() {
        assert_eq!(
            draft_from_editor("line one\r\nline two\r\n"),
            "line one\nline two"
        );
        assert_eq!(draft_from_editor("kept\n\n"), "kept\n");
        assert_eq!(draft_from_editor("no newline"), "no newline");
    }

    #[test]
    fn simple_flag() {
        assert_eq!(split_shell_words("code -w"), ["code", "-w"]);
    }

    #[test]
    fn double_quoted_arg_preserves_space() {
        assert_eq!(
            split_shell_words(r#"code --profile "Work Profile" -w"#),
            ["code", "--profile", "Work Profile", "-w"]
        );
    }

    #[test]
    fn double_quoted_program_with_spaces() {
        assert_eq!(
            split_shell_words(r#""/path with spaces/editor" --wait"#),
            ["/path with spaces/editor", "--wait"]
        );
    }

    #[test]
    fn backslash_escaped_spaces() {
        assert_eq!(
            split_shell_words(r"/path\ with\ spaces/editor"),
            ["/path with spaces/editor"]
        );
    }

    #[test]
    fn single_quoted_program() {
        assert_eq!(split_shell_words("'my editor' -x"), ["my editor", "-x"]);
    }

    #[test]
    fn single_quotes_are_literal() {
        // No escapes inside single quotes: backslash stays verbatim.
        assert_eq!(split_shell_words(r"'a\b'"), [r"a\b"]);
    }

    #[test]
    fn escaped_quote_inside_double_quotes() {
        assert_eq!(split_shell_words(r#""a\"b""#), [r#"a"b"#]);
    }

    #[test]
    fn empty_string_yields_no_words() {
        assert!(split_shell_words("").is_empty());
    }

    #[test]
    fn whitespace_only_yields_no_words() {
        assert!(split_shell_words("   \t  ").is_empty());
    }

    #[test]
    fn unterminated_double_quote_emits_rest() {
        // Documented choice: leftover text is emitted, not rejected.
        assert_eq!(
            split_shell_words(r#"code "Work Profile"#),
            ["code", "Work Profile"]
        );
    }

    #[test]
    fn unterminated_single_quote_emits_rest() {
        assert_eq!(split_shell_words("code 'work"), ["code", "work"]);
    }

    #[test]
    fn trailing_backslash_is_literal() {
        assert_eq!(split_shell_words(r"code\"), [r"code\"]);
    }
}

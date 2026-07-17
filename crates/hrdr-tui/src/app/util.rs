//! Free helper functions with no `App` receiver.

/// Split a command string into words using POSIX-ish shell rules.
///
/// Handles the quoting seen in `$EDITOR`/`$VISUAL` values without shelling
/// out to a real shell (which would add injection/quoting hazards):
/// - whitespace separates words;
/// - double quotes group text, honoring backslash escapes for `"` and `\`;
/// - single quotes group text literally (no escapes recognized inside);
/// - a backslash outside quotes escapes the next character verbatim.
///
/// Unterminated quote handling: if the string ends while still inside a
/// quote (or right after a trailing backslash), the accumulated text is
/// emitted as-is rather than erroring. `$EDITOR` is trusted local config,
/// so best-effort recovery beats failing to launch the editor.
///
/// Windows note: the same parser applies. `cmd`-style `%VAR%` expansion and
/// caret (`^`) quoting are out of scope; use forward slashes / this quoting.
pub(crate) fn split_shell_words(input: &str) -> Vec<String> {
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
                // Double quotes: backslash escapes `"` and `\` only.
                while let Some(dc) = chars.next() {
                    match dc {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some('"') | Some('\\') => cur.push(chars.next().unwrap()),
                            _ => cur.push('\\'),
                        },
                        _ => cur.push(dc),
                    }
                }
            }
            '\\' => {
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

/// Run `$VISUAL`/`$EDITOR` (falling back to `vi`) on `path`, inheriting stdio.
/// The command string may carry quoted args and paths with spaces (e.g.
/// `code --profile "Work Profile" -w`); it is parsed with [`split_shell_words`].
pub(crate) fn run_editor(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = split_shell_words(&editor);
    // Empty/whitespace-only value parses to no words; fall back to `vi`.
    let program = if parts.is_empty() {
        "vi".to_string()
    } else {
        parts.remove(0)
    };
    std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
}

#[cfg(test)]
mod tests {
    use super::split_shell_words;

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

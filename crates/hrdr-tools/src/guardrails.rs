//! Shell-command guardrails: mechanical enforcement of the rules the system
//! prompt states. A prompt line alone is unreliable (models drift under
//! context pressure); rejecting the command with a corrective error teaches
//! the model at exactly the moment it matters.
//!
//! The default set blocks the classic git foot-guns (blanket staging,
//! force-push, hook skipping, destructive resets, interactive commands that
//! need a TTY). Users can add project-specific rules via `[[guardrails]]`
//! entries in `config.toml`.

use regex::Regex;

/// One command rule: a regex matched against the whole shell command line and
/// the corrective message returned to the model when it matches.
#[derive(Debug, Clone)]
pub struct Guardrail {
    pub pattern: Regex,
    pub message: String,
}

impl Guardrail {
    /// Build from a user-supplied pattern; `Err` on invalid regex.
    pub fn new(pattern: &str, message: impl Into<String>) -> Result<Self, regex::Error> {
        Ok(Self {
            pattern: Regex::new(pattern)?,
            message: message.into(),
        })
    }
}

/// The built-in rules. Patterns are matched anywhere in the command string
/// (compound `a && b` commands included). Kept deliberately narrow: they block
/// the exact foot-gun spellings, not whole subcommands.
pub fn default_guardrails() -> Vec<Guardrail> {
    // (pattern, message); patterns are hand-checked below in the unit tests.
    // NB: the regex crate has no lookaround — `--force` must not also match
    // `--force-with-lease`, so it's anchored to a non-word boundary manually.
    let rules: &[(&str, &str)] = &[
        (
            r"\bgit\s+add\b[^&|;]*(\s-[a-zA-Z]*A|\s--all\b|\s\.(/)?(\s|$|['\x22;&|]))",
            "blanket staging is disabled — stage the files you actually changed: `git add <path> …`",
        ),
        (
            r"\bgit\s+push\b[^&|;]*\s(--force(\s|$|['\x22;&|])|-[a-zA-Z]*f\b)",
            "force-push is disabled — if the remote rejected the push, reconcile with fetch/rebase instead",
        ),
        (
            r"\bgit\s+commit\b[^&|;]*\s(--no-verify\b|-[a-zA-Z]*n[a-zA-Z]*\b)",
            "skipping commit hooks is disabled — fix what the hook reports instead",
        ),
        (
            r"\bgit\s+push\b[^&|;]*\s--no-verify\b",
            "skipping push hooks is disabled — fix what the hook reports instead",
        ),
        (
            r"\bgit\s+reset\s+--hard\b",
            "`git reset --hard` discards uncommitted work — ask the user before running destructive git commands",
        ),
        (
            r"\bgit\s+clean\b[^&|;]*\s-[a-zA-Z]*[fd]",
            "`git clean` deletes untracked files — ask the user before running destructive git commands",
        ),
        (
            r"\bgit\s+(checkout|restore)\s+(--\s+)?\.(/)?(\s|$|['\x22;&|])",
            "this discards all uncommitted changes — ask the user before running destructive git commands",
        ),
        (
            r"\bgit\s+(rebase|add|commit)\b[^&|;]*\s(--interactive\b|-[a-zA-Z]*i\b)",
            "interactive git commands need a TTY, which this shell doesn't have — use the non-interactive form",
        ),
    ];
    rules
        .iter()
        .map(|(p, m)| Guardrail {
            pattern: Regex::new(p).expect("built-in guardrail regex"),
            message: (*m).to_string(),
        })
        .collect()
}

/// First matching rule's message, if `command` trips any guardrail. Quoted
/// spans are blanked before matching so string *arguments* that merely mention
/// a blocked command (e.g. `rg 'git add -A'`) don't false-positive.
pub fn check_guardrails<'a>(command: &str, rails: &'a [Guardrail]) -> Option<&'a str> {
    let stripped = strip_quoted(command);
    rails
        .iter()
        .find(|r| r.pattern.is_match(&stripped))
        .map(|r| r.message.as_str())
}

/// Replace the contents of single-/double-quoted spans with spaces (quotes
/// kept, backslash escapes honored inside double quotes). Unterminated quotes
/// blank to the end of the string — conservative in the blocking direction is
/// wrong here, so an unterminated quote can't hide a real command anyway (the
/// shell would error before running it).
fn strip_quoted(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                out.push(c);
                while let Some(q) = chars.next() {
                    if q == '\\' && c == '"' {
                        chars.next();
                        out.push(' ');
                        out.push(' ');
                        continue;
                    }
                    if q == c {
                        out.push(c);
                        break;
                    }
                    out.push(' ');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(cmd: &str) -> bool {
        check_guardrails(cmd, &default_guardrails()).is_some()
    }

    #[test]
    fn blanket_staging_blocked() {
        assert!(blocked("git add -A"));
        assert!(blocked("git add --all"));
        assert!(blocked("git add ."));
        assert!(blocked("git add ./"));
        assert!(blocked("git add -Av"));
        assert!(blocked("cd repo && git add -A && git commit -m x"));
        // Explicit paths are fine — including dotfiles and dotted dirs.
        assert!(!blocked("git add src/main.rs Cargo.toml"));
        assert!(!blocked("git add .gitignore"));
        assert!(!blocked("git add ./src/main.rs"));
        assert!(!blocked("git add .github/workflows/ci.yml"));
    }

    #[test]
    fn force_push_blocked_but_lease_allowed() {
        assert!(blocked("git push --force"));
        assert!(blocked("git push -f origin main"));
        assert!(blocked("git push origin main --force"));
        assert!(!blocked("git push --force-with-lease"));
        assert!(!blocked("git push origin main"));
    }

    #[test]
    fn hook_skips_blocked() {
        assert!(blocked("git commit --no-verify -m x"));
        assert!(blocked("git commit -nm x"));
        assert!(blocked("git push --no-verify"));
        // `git push -n` is --dry-run, not --no-verify: allowed.
        assert!(!blocked("git push -n origin main"));
        assert!(!blocked("git commit -m 'fix: thing'"));
    }

    #[test]
    fn destructive_blocked() {
        assert!(blocked("git reset --hard HEAD~1"));
        assert!(blocked("git clean -fd"));
        assert!(blocked("git checkout ."));
        assert!(blocked("git checkout -- ."));
        assert!(blocked("git restore ."));
        assert!(!blocked("git reset HEAD~1"));
        assert!(!blocked("git checkout main"));
        assert!(!blocked("git restore src/lib.rs"));
    }

    #[test]
    fn interactive_blocked() {
        assert!(blocked("git rebase -i HEAD~3"));
        assert!(blocked("git add -i"));
        assert!(blocked("git rebase --interactive main"));
        assert!(!blocked("git rebase main"));
    }

    #[test]
    fn unrelated_commands_pass() {
        assert!(!blocked("cargo test"));
        assert!(!blocked("ls -la"));
        assert!(!blocked("git status"));
        assert!(!blocked("git diff --stat"));
        assert!(!blocked("rg -n 'git add -A' docs/")); // mentions, not runs
    }

    #[test]
    fn custom_rule_applies() {
        let rails = vec![Guardrail::new(r"\brm\s+-rf\s+/", "no").unwrap()];
        assert_eq!(check_guardrails("rm -rf /tmp/x", &rails), Some("no"));
        assert_eq!(check_guardrails("rm foo", &rails), None);
    }
}

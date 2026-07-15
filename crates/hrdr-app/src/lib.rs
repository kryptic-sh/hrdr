//! `hrdr-app` — the UI-agnostic application core shared by hrdr's frontends.
//!
//! Logic that is identical regardless of how it's rendered lives here so the TUI
//! (`hrdr-tui`) and the headless runner share one implementation instead of each
//! reimplementing it. Today: the slash-command registry, alias resolution, and
//! "quit command" detection. More (help metadata is already here) will move in
//! as the frontends converge.

// Every test in this crate — including one written tomorrow by someone who read none
// of this — runs with `$HOME` and the XDG roots pointed at a throwaway directory. The
// `extern crate` is what links `hrdr-test-support`'s life-before-main ctor into this
// test binary; rustc drops a dependency nothing references, and a dropped ctor is a
// test writing the developer's real sessions. Do not remove it.
#[cfg(test)]
extern crate hrdr_test_support;

mod commands;
mod completion;
mod config;
mod effort;
mod format;
mod highlight;
mod history;
mod login;
mod palette;
mod pane;
mod session;
mod sessions;
mod skills;
mod status;
mod subagents;
mod themes;
mod transcript;
mod util;
pub use commands::*;
pub use completion::*;
pub use config::*;
pub use effort::*;
pub use format::*;
pub use highlight::*;
pub use history::*;
pub use login::*;
pub use palette::*;
pub use pane::{
    Pane, PaneId, PaneRow, PaneSet, PaneStatus, PaneView, apply_event, pane_row_marker, pane_rows,
};
pub use session::*;
pub use sessions::*;
pub use skills::*;
pub use status::*;
pub use subagents::*;
pub use themes::*;
pub use transcript::*;
pub use util::*;

/// The slash commands, as `(name, one-line description)`. Frontends render this
/// however they like (a completion popup, a `/` menu, a help screen).
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a fresh conversation (optional name)"),
    ("/compact", "summarize the conversation to reclaim context"),
    ("/resume", "resume a session (picker, or id/name)"),
    ("/rename", "rename the current session"),
    ("/model", "browse & switch model/provider (picker)"),
    ("/login", "set up a provider + API key (wizard)"),
    ("/theme", "switch theme (picker, name/path, reset)"),
    ("/cwd", "show or change working directory"),
    ("/tools", "list available tools"),
    ("/skills", "list custom :skills (prompt templates)"),
    ("/prompt", "show the rendered system prompt"),
    ("/guardrails", "list active shell guardrails"),
    ("/expand", "expand tool output (last, or 'all'/'off')"),
    ("/init", "analyze the project and write an AGENTS.md"),
    ("/add", "attach a file (or type @path inline)"),
    ("/diff", "show git diff of the working tree"),
    ("/thinking", "show/hide model reasoning (on|off)"),
    ("/reasoning", "alias of /thinking"),
    ("/timestamps", "set timestamps (none|relative|exact)"),
    ("/statusbar", "set status bar (none|truncate|wrap)"),
    ("/todo-ttl", "turns a finished todo stays shown"),
    ("/reload", "reload AGENTS.md + config"),
    ("/temp", "show or set temperature"),
    ("/effort", "reasoning effort (picker)"),
    ("/status", "session info"),
    ("/cost", "session token usage"),
    ("/doctor", "check health: endpoint, deps, config"),
    ("/goto", "jump to message N or time (5m/1h/top/end)"),
    ("/find", "jump to text (or 'clear' to drop search)"),
    ("/next", "jump to next /find match"),
    ("/prev", "jump to previous /find match"),
    ("/copy", "copy reply (or 'code' / 'all' / 'msg N[-M]')"),
    ("/export", "write transcript to a file ([--json] [file])"),
    ("/paste", "paste clipboard (file path → attach)"),
    ("/edit", "open a file in $EDITOR"),
    ("/help", "list commands"),
    ("/exit", "quit"),
    // Aliases for users switching from other agents (resolved by resolve_alias).
    ("/clear", "alias of /new (optional name)"),
    ("/reset", "alias of /new"),
    ("/cd", "alias of /cwd"),
    ("/info", "alias of /status"),
    ("/continue", "alias of /resume"),
    ("/sessions", "alias of /resume"),
    ("/summarize", "alias of /compact"),
    ("/usage", "alias of /cost"),
    ("/health", "alias of /doctor"),
];

/// Commands grouped by theme, for a readable help listing.
pub const HELP_GROUPS: &[(&str, &[&str])] = &[
    (
        "Session",
        &[
            "/new", "/resume", "/rename", "/compact", "/status", "/goto", "/find", "/next", "/prev",
        ],
    ),
    (
        "Model & sampling",
        &["/model", "/login", "/temp", "/effort", "/thinking"],
    ),
    (
        "Files & context",
        &[
            "/init", "/add", "/edit", "/diff", "/cwd", "/tools", "/expand", "/paste",
        ],
    ),
    ("Reply", &["/copy", "/export", "/cost"]),
    (
        "Appearance",
        &["/theme", "/timestamps", "/statusbar", "/todo-ttl"],
    ),
    (
        "Other",
        &["/skills", "/reload", "/help", "/doctor", "/exit"],
    ),
];

/// Whether `cmd` (with or without the leading `/`; aliases welcome) is a
/// registered slash command at all — used by frontends to tell "command I
/// don't support" apart from "not a command, send it to the model".
pub fn is_known_command(cmd: &str) -> bool {
    let c = resolve_alias(cmd.trim().trim_start_matches('/'));
    SLASH_COMMANDS
        .iter()
        .any(|(n, _)| resolve_alias(n.trim_start_matches('/')) == c)
}

/// Resolve a slash-command alias to its canonical name (case-insensitive), so
/// users coming from other agents can use familiar names. Unknown names pass
/// through unchanged.
pub fn resolve_alias(cmd: &str) -> &str {
    match cmd.to_ascii_lowercase().as_str() {
        // Claude Code / aider names for starting over; /new is opencode's.
        "clear" | "reset" => "new",
        // aider/shell-style directory change.
        "cd" => "cwd",
        // hrdr's pre-Claude-style name for the session summary.
        "info" => "status",
        // opencode/Claude Code resume; /sessions is the picker too now.
        "continue" | "sessions" => "resume",
        // descriptive name for compaction.
        "summarize" | "summary" => "compact",
        // help variants.
        "commands" | "?" => "help",
        // usage / health variants.
        "usage" => "cost",
        "health" => "doctor",
        _ => cmd,
    }
}

/// The grouped, aligned `/help` body: a `Commands` header followed by each
/// `HELP_GROUPS` section with its commands and descriptions, only listing
/// commands `show` accepts — so a frontend's `/help` advertises exactly what
/// it supports (via [`CommandHost::supports_command`]). Groups
/// left empty are omitted. Frontends append their own keybinding "Tips:" tail
/// (those keys differ per frontend).
pub fn help_body_for(show: impl Fn(&str) -> bool) -> String {
    let desc = |name: &str| {
        SLASH_COMMANDS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, d)| *d)
            .unwrap_or("")
    };
    // Column width from the longest command so long names (`/timestamps`)
    // never run into their descriptions.
    let width = HELP_GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|n| n.len())
        .max()
        .unwrap_or(0)
        + 2;
    let mut s = String::from("Commands");
    for (group, names) in HELP_GROUPS {
        let shown: Vec<&&str> = names.iter().filter(|n| show(n)).collect();
        if shown.is_empty() {
            continue;
        }
        s.push_str(&format!("\n\n{group}"));
        for name in shown {
            s.push_str(&format!("\n  {name:<width$}{}", desc(name)));
        }
    }
    s
}

/// Whether a submitted line is a common "quit the session" command, matched
/// across popular CLIs/REPLs/editors so users feel at home: bare `exit`/`quit`,
/// the `/exit` `/quit` `/bye` slash family, and vim's `:q` family.
pub fn is_quit_command(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "exit"
            | "quit"
            | "q"
            | "bye"
            | "exit()"
            | "quit()"
            | "/exit"
            | "/quit"
            | "/q"
            | "/bye"
            | "/stop"
            | ":q"
            | ":q!"
            | ":qa"
            | ":qa!"
            | ":wq"
            | ":x"
            | ":exit"
            | ":quit"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_canonical() {
        assert_eq!(resolve_alias("clear"), "new");
        assert_eq!(resolve_alias("RESET"), "new"); // case-insensitive
        assert_eq!(resolve_alias("cd"), "cwd");
        assert_eq!(resolve_alias("info"), "status");
        assert_eq!(resolve_alias("continue"), "resume");
        assert_eq!(resolve_alias("sessions"), "resume");
        assert_eq!(resolve_alias("summarize"), "compact");
        assert_eq!(resolve_alias("?"), "help");
        assert_eq!(resolve_alias("model"), "model"); // unknown passes through
    }

    #[test]
    fn recognizes_common_quit_commands() {
        for cmd in [
            "exit",
            "quit",
            "q",
            "bye",
            "/exit",
            "/quit",
            "/bye",
            ":q",
            ":qa",
            ":wq",
            ":x",
            "EXIT",
            "  /quit  ",
        ] {
            assert!(is_quit_command(cmd), "{cmd:?} should quit");
        }
    }

    #[test]
    fn leaves_normal_messages_alone() {
        for msg in [
            "exit the loop early",
            "how do I quit vim?",
            "q1 results",
            "fix bye-bug",
        ] {
            assert!(!is_quit_command(msg), "{msg:?} should NOT quit");
        }
    }

    #[test]
    fn known_command_classification() {
        assert!(is_known_command("/new")); // alias entries count
        assert!(is_known_command("model"));
        assert!(!is_known_command("/frobnicate"));
    }

    #[test]
    fn every_help_group_command_exists() {
        // Guards against a help group referencing a command not in the registry.
        for (_, names) in HELP_GROUPS {
            for name in *names {
                assert!(
                    SLASH_COMMANDS.iter().any(|(n, _)| n == name),
                    "help group references unknown command {name}"
                );
            }
        }
    }
}

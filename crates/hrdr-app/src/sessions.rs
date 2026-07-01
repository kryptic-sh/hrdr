//! Session-list formatting shared by hrdr's frontends. The listing text (which
//! sessions to show for the current cwd, or all, and how to render each row) is
//! representation-independent, so both the TUI's `/sessions` and the GUI's build
//! it identically. Resuming itself is per-frontend (it rebuilds each one's own
//! transcript representation), so it stays in the frontends.

/// The `/sessions` listing as a display string. With `all`, every directory's
/// sessions are shown (each row tagged with its cwd); otherwise only those whose
/// cwd matches `cwd`. Returns a friendly empty-state message when there are none.
pub fn session_list_text(all: bool, cwd: &str) -> String {
    let cur = hrdr_agent::cwd_slug(cwd);
    let sessions: Vec<_> = hrdr_agent::list_sessions()
        .into_iter()
        .filter(|m| all || hrdr_agent::cwd_slug(&m.cwd) == cur)
        .collect();
    if sessions.is_empty() {
        return if all {
            format!(
                "no saved sessions in {}",
                hrdr_agent::sessions_dir().display()
            )
        } else {
            "no saved sessions for this directory (try /sessions --all)".to_string()
        };
    }
    let mut s = if all {
        String::from("all sessions (resume by id or name):")
    } else {
        String::from("sessions here (resume by id or name; /sessions --all for every dir):")
    };
    for m in sessions {
        if all {
            s.push_str(&format!("\n  {} — {}  [{}]", m.id, m.name, m.cwd));
        } else {
            s.push_str(&format!("\n  {} — {}", m.id, m.name));
        }
    }
    s
}

/// Whether a `/sessions` argument requests every directory's sessions.
pub fn sessions_all_flag(arg: &str) -> bool {
    matches!(arg.trim(), "--all" | "-a" | "all")
}

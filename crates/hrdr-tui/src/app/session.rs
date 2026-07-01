//! Session persistence, restore, and transcript rebuild.

use std::collections::HashMap;

use hrdr_agent::{Message, MessageRole, Session};

use super::*;

impl super::App {
    /// On startup, resume the most recent saved session for the current
    /// directory (if any). No match → leave the fresh session as-is.
    pub(super) fn auto_resume_latest(&mut self) {
        let cwd = self.current_cwd();
        let cur = hrdr_agent::cwd_slug(&cwd);
        let Some(meta) = hrdr_agent::list_sessions()
            .into_iter()
            .find(|m| hrdr_agent::cwd_slug(&m.cwd) == cur)
        else {
            return; // nothing saved here yet — start fresh
        };
        let Ok(session) = Session::load_path(&meta.path) else {
            return;
        };
        // Skip empty sessions (system prompt only).
        if session.messages.len() <= 1 {
            return;
        }
        if let Ok(mut a) = self.agent.try_lock() {
            a.set_messages(session.messages.clone());
            a.set_model(session.model.clone());
        }
        self.model = session.model.clone();
        self.rebuild_transcript(&session.messages);
        self.session_id = Some(meta.id);
        self.session_label = Some(session.name.clone());
        self.push_entry(Entry::System(format!(
            "resumed most recent session '{}' ({} messages) — /clear to start fresh",
            session.name,
            session.messages.len()
        )));
    }
    /// Persist the conversation. Sessions auto-save continuously: any non-empty
    /// conversation is written to disk, with a stable file id assigned (from the
    /// name) on first save. Called after every completed turn, `/undo`,
    /// `/retry`, and `/rename`.
    pub(super) fn autosave(&mut self) {
        let snap = self
            .agent
            .try_lock()
            .ok()
            .map(|a| (a.messages_owned(), a.cwd()));
        let Some((msgs, cwd)) = snap else {
            return;
        };
        // Non-empty == has at least one user message.
        if !msgs.iter().any(|m| m.role == MessageRole::User) {
            return;
        }
        let name = self
            .session_label
            .clone()
            .unwrap_or_else(|| session_name_from(&msgs));
        // Notify once, when the session is first created.
        if self.session_id.is_none() {
            let id = hrdr_agent::unique_session_id(&cwd.display().to_string(), &name);
            self.push_entry(Entry::System(format!(
                "session saved as '{id}' — /resume {id}"
            )));
            self.session_id = Some(id);
        }
        let id = self.session_id.clone().unwrap_or_else(|| name.clone());
        let s = Session::new(
            name,
            self.model.clone(),
            self.base_url.clone(),
            cwd.display().to_string(),
            msgs,
        );
        let _ = s.save(&id); // best-effort; silent
    }
    pub(super) fn rename_session(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /rename <name>");
            return;
        }
        self.session_label = Some(arg.to_string());
        self.autosave(); // persist the new name (no-op while still empty)
        self.system(format!("session renamed → {arg}"));
    }
    pub(super) fn resume_session(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /resume <id-or-name>  (see /sessions)");
            return;
        }
        if self.running {
            self.system("can't resume while a turn is running");
            return;
        }
        // Match by file id first, then by display name (e.g. after /rename).
        let cwd = self.current_cwd();
        let Some((id, session)) = hrdr_agent::resolve_session(&cwd, arg) else {
            self.system(format!("no session matching '{arg}' (see /sessions)"));
            return;
        };
        let count = session.messages.len();
        if let Ok(mut a) = self.agent.try_lock() {
            a.set_messages(session.messages.clone());
            a.set_model(session.model.clone());
        }
        self.model = session.model.clone();
        self.rebuild_transcript(&session.messages);
        self.session_id = Some(id.clone());
        self.session_label = Some(session.name.clone());
        self.scroll_offset = 0;
        self.system(format!("resumed '{}' ({count} messages)", session.name));
        // Switch hrdr's tools to the session's working directory (in-process
        // only — the parent shell is untouched).
        if !session.cwd.is_empty() && session.cwd != cwd {
            let target = std::path::PathBuf::from(&session.cwd);
            if target.is_dir() {
                self.apply_cwd(target.clone());
                self.system(format!("cwd → {}", target.display()));
            } else {
                self.system(format!(
                    "note: session cwd {} no longer exists; staying in {cwd}",
                    session.cwd
                ));
            }
        }
        if session.base_url != self.base_url {
            self.system(format!(
                "note: session endpoint was {} (current: {})",
                session.base_url, self.base_url
            ));
        }
    }
    pub(super) fn list_sessions_cmd(&mut self, arg: &str) {
        let all = matches!(arg.trim(), "--all" | "-a" | "all");
        let cur = hrdr_agent::cwd_slug(&self.current_cwd());
        let sessions: Vec<_> = hrdr_agent::list_sessions()
            .into_iter()
            .filter(|m| all || hrdr_agent::cwd_slug(&m.cwd) == cur)
            .collect();
        if sessions.is_empty() {
            self.system(if all {
                format!(
                    "no saved sessions in {}",
                    hrdr_agent::sessions_dir().display()
                )
            } else {
                "no saved sessions for this directory (try /sessions --all)".to_string()
            });
            return;
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
        self.system(s);
    }
    /// Rebuild the display transcript from a restored message history.
    fn rebuild_transcript(&mut self, msgs: &[Message]) {
        self.clear_transcript();
        // Map tool_call_id → (result, ok) from the tool-result messages.
        let mut results: HashMap<String, (String, bool)> = HashMap::new();
        for m in msgs {
            if m.role == MessageRole::Tool
                && let (Some(id), Some(content)) = (&m.tool_call_id, &m.content)
            {
                let ok = !content.starts_with("Error:");
                results.insert(id.clone(), (content.clone(), ok));
            }
        }
        for m in msgs {
            match m.role {
                MessageRole::User => {
                    if let Some(c) = &m.content {
                        self.push_entry(Entry::User(c.clone()));
                    }
                }
                MessageRole::Assistant => {
                    if let Some(c) = &m.content
                        && !c.is_empty()
                    {
                        self.push_entry(Entry::Assistant(c.clone()));
                    }
                    for call in m.tool_calls.iter().flatten() {
                        let (result, ok) = results.get(&call.id).cloned().unwrap_or_default();
                        self.push_entry(Entry::Tool {
                            id: call.id.clone(),
                            name: call.function.name.clone(),
                            args: call.function.arguments.clone(),
                            result,
                            ok,
                            done: true,
                            expanded: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

/// A short session name derived from the first user message.
pub(super) fn session_name_from(msgs: &[Message]) -> String {
    msgs.iter()
        .find(|m| m.role == MessageRole::User)
        .and_then(|m| m.content.as_deref())
        .map(|c| {
            c.lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(60)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "untitled".to_string())
}

//! Shared slash-command layer. The command *implementations* live here, behind
//! the [`CommandHost`] trait, so both frontends (TUI, GUI) drive the exact same
//! logic and gain new commands for free — a frontend just implements the host
//! capabilities (emit a line, access the agent, clipboard, sessions, …) and
//! calls [`dispatch`]. Frontend-coupled commands (scrolling, find/goto, expand,
//! theme/timestamps, editor) stay in the frontends and are handled before
//! delegating here.
//!
//! Async work (network, subprocess, filesystem, agent lock) is expressed as a
//! [`LineFuture`] the host spawns; its returned string (if non-empty) is shown
//! as a system line. This keeps the layer uniform across the sync-polled TUI and
//! the async-locked GUI, which both hold the agent as `Arc<tokio::sync::Mutex>`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use hrdr_agent::{Agent, Message, MessageRole, Session};
use tokio::sync::Mutex;

/// A future producing a system line to display (empty = show nothing). The host
/// spawns it on its runtime and pipes the result to its transcript.
pub type LineFuture = Pin<Box<dyn Future<Output = String> + Send>>;

/// The capabilities a frontend exposes so the shared commands can drive it.
pub trait CommandHost {
    /// Emit a system line immediately (on the UI thread).
    fn info(&mut self, line: String);
    /// Spawn `fut`; when it resolves, show its non-empty string as a system line.
    fn spawn_line(&self, fut: LineFuture);
    /// The shared agent handle (for async reads/mutations).
    fn agent(&self) -> Arc<Mutex<Agent>>;
    /// Working directory the tools operate in.
    fn cwd(&self) -> PathBuf;
    /// Current endpoint base URL (recorded into saved sessions).
    fn base_url(&self) -> String;

    /// The displayed model name.
    fn model(&self) -> String;
    /// Update the displayed model (the agent is switched separately by dispatch).
    fn set_model(&mut self, model: String);

    /// Whether `<think>` reasoning is shown.
    fn show_thinking(&self) -> bool;
    /// Toggle reasoning display (persisting if the frontend supports it).
    fn set_show_thinking(&mut self, on: bool);

    /// Reset to a fresh conversation (clear transcript + agent history + session).
    fn clear_conversation(&mut self);

    /// The active session's file id, if one has been assigned.
    fn session_id(&self) -> Option<String>;
    /// Override the session's display name (used by `/rename`).
    fn set_session_label(&mut self, name: String);
    /// Persist the current conversation (assigning/reusing the session id).
    fn autosave(&mut self);
    /// Restore a resolved session (rebuild the transcript, adopt id/model/label).
    fn resume(&mut self, id: String, session: Session);

    /// Copy `text` to the OS clipboard, returning a status line.
    fn copy_to_clipboard(&mut self, text: &str, label: &str) -> String;
    /// The most recent assistant reply, if any.
    fn last_reply(&self) -> Option<String>;
    /// The whole transcript as plain text (for `/copy all`).
    fn transcript_text(&self) -> String;

    /// Frontend-specific keybinding tips appended to `/help`.
    fn help_tips(&self) -> Option<String> {
        None
    }
}

/// Handle a `/…` command shared by both frontends. Returns `true` if it was a
/// recognized command (and thus shouldn't be sent to the model). Unknown input
/// returns `false` so the caller can pass it to the model or handle it locally.
pub fn dispatch(host: &mut dyn CommandHost, input: &str) -> bool {
    let Some(rest) = input.strip_prefix('/') else {
        return false;
    };
    let mut parts = rest.splitn(2, char::is_whitespace);
    let cmd = crate::resolve_alias(parts.next().unwrap_or(""));
    let arg = parts.next().unwrap_or("").trim().to_string();
    match cmd {
        "help" => {
            let mut s = crate::help_body();
            if let Some(tips) = host.help_tips() {
                s.push_str("\n\n");
                s.push_str(&tips);
            }
            host.info(s);
        }
        "clear" => {
            host.clear_conversation();
            host.info("conversation cleared".to_string());
        }
        "model" => {
            if arg.is_empty() {
                host.info(format!("model: {}", host.model()));
            } else {
                host.set_model(arg.clone());
                let agent = host.agent();
                let name = arg.clone();
                host.spawn_line(Box::pin(async move {
                    agent.lock().await.set_model(name);
                    String::new()
                }));
                host.info(format!("model → {arg}"));
            }
        }
        "models" => {
            let agent = host.agent();
            host.info("fetching models…".to_string());
            host.spawn_line(Box::pin(async move {
                let client = agent.lock().await.client();
                match client.list_models().await {
                    Ok(m) if !m.is_empty() => format!("models:\n  {}", m.join("\n  ")),
                    Ok(_) => "endpoint reported no models".to_string(),
                    Err(e) => format!("models error: {e}"),
                }
            }));
        }
        "tools" => {
            let agent = host.agent();
            host.spawn_line(Box::pin(async move {
                let tools = agent.lock().await.tools();
                let mut msg = format!("{} tools:", tools.len());
                for (name, desc) in tools {
                    msg.push_str(&format!("\n  {name:<12}{desc}"));
                }
                msg
            }));
        }
        "info" => {
            let agent = host.agent();
            let model = host.model();
            let cwd = host.cwd();
            host.spawn_line(Box::pin(async move {
                let a = agent.lock().await;
                format!(
                    "model: {model}\nmessages: {}\ncwd: {}",
                    a.message_count(),
                    cwd.display()
                )
            }));
        }
        "copy" => {
            let (text, label) = match arg.to_ascii_lowercase().as_str() {
                "" | "reply" | "last" => (host.last_reply(), "last reply"),
                "code" => (
                    host.last_reply()
                        .as_deref()
                        .and_then(crate::last_fenced_block),
                    "last code block",
                ),
                "all" | "transcript" => (Some(host.transcript_text()), "transcript"),
                _ => {
                    host.info("usage: /copy [code | all]".to_string());
                    return true;
                }
            };
            let line = match text {
                Some(t) if !t.is_empty() => host.copy_to_clipboard(&t, label),
                _ => format!("nothing to copy ({label})"),
            };
            host.info(line);
        }
        "export" => {
            let agent = host.agent();
            let cwd = host.cwd();
            let arg = arg.clone();
            host.spawn_line(Box::pin(async move {
                let msgs = agent.lock().await.messages_owned();
                match export_conversation(&msgs, &cwd, &arg) {
                    Ok((path, lines)) => {
                        format!("exported transcript to {} ({lines} lines)", path.display())
                    }
                    Err(e) => format!("export failed: {e}"),
                }
            }));
        }
        "rename" => {
            if arg.is_empty() {
                host.info("usage: /rename <name>".to_string());
                return true;
            }
            host.set_session_label(arg.clone());
            host.autosave();
            host.info(format!("session renamed → {arg}"));
        }
        "diff" => {
            let cwd = host.cwd();
            host.spawn_line(Box::pin(async move {
                match git_working_diff(&cwd).await {
                    Ok(d) if d.trim().is_empty() => "git diff: no changes".to_string(),
                    Ok(d) => d,
                    Err(e) => format!("git diff failed: {e}"),
                }
            }));
        }
        "thinking" | "reasoning" | "think" => {
            let on = if arg.is_empty() {
                !host.show_thinking()
            } else if let Some(b) = hrdr_agent::parse_env_bool(&arg) {
                b
            } else {
                host.info("usage: /thinking [on | off]".to_string());
                return true;
            };
            host.set_show_thinking(on);
            host.info(
                if on {
                    "thinking shown"
                } else {
                    "thinking hidden"
                }
                .to_string(),
            );
        }
        "sessions" => {
            let all = crate::sessions_all_flag(&arg);
            host.info(crate::session_list_text(
                all,
                &host.cwd().display().to_string(),
            ));
        }
        "resume" | "load" => {
            if arg.is_empty() {
                host.info("usage: /resume <id or name> (see /sessions)".to_string());
                return true;
            }
            match hrdr_agent::resolve_session(&host.cwd().display().to_string(), &arg) {
                Some((id, session)) => host.resume(id, session),
                None => host.info(format!("no session matching '{arg}' (see /sessions)")),
            }
        }
        _ => return false,
    }
    true
}

// ---- representation-independent command cores ----

/// The working-tree `git diff` for `cwd` (stdout on success, stderr message on
/// failure). Shared by `/diff`.
pub async fn git_working_diff(cwd: &Path) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .arg("diff")
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Write the conversation to a file per a `/export [--json] [file]` argument,
/// returning the path written and its line count. With no file, a timestamped
/// `hrdr-transcript-<date>.{md,json}` in `cwd` is used.
pub fn export_conversation(
    msgs: &[Message],
    cwd: &Path,
    arg: &str,
) -> Result<(PathBuf, usize), String> {
    let mut json = false;
    let mut file: Option<&str> = None;
    for tok in arg.split_whitespace() {
        if tok == "--json" {
            json = true;
        } else if file.is_none() {
            file = Some(tok);
        }
    }
    let path = match file {
        Some(f) => crate::resolve_under(cwd, f),
        None => {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let ext = if json { "json" } else { "md" };
            cwd.join(format!("hrdr-transcript-{stamp}.{ext}"))
        }
    };
    let content = if json {
        conversation_to_json(msgs)
    } else {
        conversation_to_markdown(msgs)
    };
    std::fs::write(&path, &content).map_err(|e| e.to_string())?;
    Ok((path, content.lines().count()))
}

/// The conversation's user/assistant turns as Markdown.
pub fn conversation_to_markdown(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        match m.role {
            MessageRole::User => {
                if let Some(c) = &m.content {
                    out.push_str(&format!("## User\n{c}\n\n"));
                }
            }
            MessageRole::Assistant => {
                if let Some(c) = &m.content
                    && !c.is_empty()
                {
                    out.push_str(&format!("## Assistant\n{c}\n\n"));
                }
            }
            _ => {}
        }
    }
    out.trim_end().to_string()
}

/// The conversation's user/assistant turns as a JSON array of `{n, role, content}`.
pub fn conversation_to_json(msgs: &[Message]) -> String {
    let mut arr = Vec::new();
    let mut num = 0;
    for m in msgs {
        let (role, content) = match m.role {
            MessageRole::User => ("user", m.content.as_deref()),
            MessageRole::Assistant => ("assistant", m.content.as_deref()),
            _ => continue,
        };
        let Some(content) = content.filter(|c| !c.is_empty()) else {
            continue;
        };
        num += 1;
        arr.push(serde_json::json!({ "n": num, "role": role, "content": content }));
    }
    serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_export_covers_only_user_assistant() {
        let msgs = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::assistant(""), // empty assistant (tool-call turn) skipped
        ];
        let md = conversation_to_markdown(&msgs);
        assert!(md.contains("## User\nhello"));
        assert!(md.contains("## Assistant\nhi there"));
        assert!(!md.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&conversation_to_json(&msgs)).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
    }
}

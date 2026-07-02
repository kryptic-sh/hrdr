//! Slash-command dispatch and the individual command handlers.

use super::*;
use crate::theme::Theme;
use hjkl_clipboard::{MimeType, Selection};
use hrdr_app::{last_fenced_block, parse_duration, resolve_alias, resolve_under};

impl super::App {
    /// Dispatch a known slash command. Returns `true` if it was a recognized
    /// command (and thus shouldn't be sent to the model); unknown `/…` input
    /// returns `false` so it goes to the model (e.g. a literal path).
    pub(super) fn handle_slash(&mut self, input: &str) -> bool {
        let Some(rest) = input.strip_prefix('/') else {
            return false;
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = resolve_alias(parts.next().unwrap_or(""));
        let arg = parts.next().unwrap_or("").trim();
        // Commands with a richer TUI rendering or that touch terminal-only state
        // are handled here; everything else falls through to the shared
        // `hrdr_app` dispatcher (so the TUI and GUI run one implementation).
        match cmd {
            "info" => self.show_info(), // richer than the shared /info
            "edit" => self.edit_file_cmd(arg),
            "init" => self.init_agents_cmd(), // reloads AGENTS.md after (pending_init)
            "reload" => self.reload_cmd(),
            "goto" => self.goto_cmd(arg),
            "find" | "search" => self.find_cmd(arg),
            "next" => self.find_cycle(true),
            "prev" | "previous" => self.find_cycle(false),
            // help, clear, model, models, tools, copy, diff, rename, thinking,
            // sessions, resume, export → shared dispatcher (TuiHost overrides
            // route /diff to the colored Entry::Diff rendering).
            _ => {
                let mut host = TuiHost { app: self };
                return hrdr_app::dispatch(&mut host, input);
            }
        }
        true
    }
    /// Full reset — as if a fresh session just opened. `Agent::clear` drops
    /// history and re-reads `AGENTS.md`; this resets the view + interaction
    /// state. The shared `/clear` command emits the confirmation line.
    pub(super) fn clear_all(&mut self) {
        // A running turn holds the agent mutex for its whole duration, so a
        // try_lock clear would silently no-op — and the turn's autosave would
        // then write the old history into a brand-new session. Cancel the turn
        // first, then clear through an awaited lock (the abort releases it at
        // the task's next yield).
        if self.running {
            self.cancel_turn();
        }
        let agent = self.agent.clone();
        tokio::spawn(async move {
            agent.lock().await.clear();
        });
        self.clear_transcript();
        self.queue.clear();
        if let Ok(mut todos) = self.todos.lock() {
            todos.clear();
        }
        self.todo_turn = 0;
        self.todo_completed_at.clear();
        self.scroll_offset = 0;
        self.max_scroll = 0;
        self.session_in = 0;
        self.session_out = 0;
        self.last_usage = None;
        self.session_id = None; // detach; next message starts a new session
        self.session_label = None;
        self.find_query = None;
        self.find_pos = 0;
        self.pending_goto = None;
        self.pending_edit = None;
        self.expand_tools = false;
    }
    /// Apply an `/expand` mode (shared dispatch parses the arg), returning the
    /// status line. `expand_tools` is the sticky all-on flag; per-entry
    /// expansion lives on the Tool entries.
    pub(super) fn apply_tool_expansion(&mut self, mode: hrdr_app::ExpandMode) -> String {
        match mode {
            hrdr_app::ExpandMode::All => {
                self.expand_tools = true;
                "tool output expanded (all)".to_string()
            }
            hrdr_app::ExpandMode::Off => {
                self.expand_tools = false;
                for e in self.transcript.iter_mut() {
                    if let Entry::Tool { expanded, .. } = e {
                        *expanded = false;
                    }
                }
                "tool output collapsed".to_string()
            }
            hrdr_app::ExpandMode::ToggleLast => {
                let last = self.transcript.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool { expanded, .. } => Some(expanded),
                    _ => None,
                });
                match last {
                    Some(expanded) => {
                        *expanded = !*expanded;
                        if *expanded {
                            "expanded last tool output".to_string()
                        } else {
                            "collapsed last tool output".to_string()
                        }
                    }
                    None => "no tool output to expand".to_string(),
                }
            }
        }
    }
    /// `/reload` — re-read config + `AGENTS.md`, applying the runtime bits that
    /// can change live; keeps the current settings if the config is invalid.
    fn reload_cmd(&mut self) {
        self.apply_config_reload(true);
        self.reload_project_docs();
    }
    /// `/init` — have the model explore the project and write an `AGENTS.md`
    /// (Claude Code / opencode style): we send it an instruction prompt and it
    /// uses its tools to analyze the repo and create the file.
    fn init_agents_cmd(&mut self) {
        if self.running {
            self.system("can't /init while a turn is running");
            return;
        }
        self.push_entry(Entry::System(
            "/init — exploring the project to write AGENTS.md…".to_string(),
        ));
        self.scroll_offset = 0;
        self.pending_init = true;
        self.launch_turn(hrdr_app::INIT_PROMPT.to_string());
    }
    fn show_info(&mut self) {
        let temp = self.with_agent(|a| a.temperature()).flatten();
        let branch = self.branch.clone().unwrap_or_else(|| "—".into());
        let ctx = match (self.last_usage, self.context_window) {
            (Some((p, _)), Some(w)) => format!("{p} / {w}"),
            (Some((p, _)), None) => p.to_string(),
            _ => "—".into(),
        };
        let session = match (&self.session_id, &self.session_label) {
            (Some(id), Some(name)) => format!("{id}  (name: {name})"),
            (Some(id), None) => id.clone(),
            (None, _) => "(unsaved — send a message to start one)".to_string(),
        };
        let info = format!(
            "session: {session}\nmodel: {}\nendpoint: {}\ncwd: {} ({branch})\ncontext: {ctx}\ntokens: ↑{} ↓{}\ntemperature: {}\neffort: {}",
            self.model,
            self.base_url,
            self.dir,
            self.session_in,
            self.session_out,
            temp.map(|t| t.to_string())
                .unwrap_or_else(|| "default".into()),
            self.effort.clone().unwrap_or_else(|| "—".into()),
        );
        self.system(info);
    }
    /// Rewind the last user turn out of the agent history + transcript,
    /// returning the user's text (shared `/undo` and `/retry` core).
    pub(super) fn rewind_last_turn(&mut self) -> Option<String> {
        let text = self
            .agent
            .try_lock()
            .ok()
            .and_then(|mut a| a.rewind_last_user())?;
        if let Some(idx) = self
            .transcript
            .iter()
            .rposition(|e| matches!(e, Entry::User(_)))
        {
            self.truncate_transcript(idx);
        }
        self.scroll_offset = 0;
        Some(text)
    }
    /// `/goto <N | 5m | 1h | top | end>` — scroll the transcript to a message
    /// number, to the message nearest a relative time ago, or to top/bottom.
    fn goto_cmd(&mut self, arg: &str) {
        let count = self.display_message_count();
        if count == 0 {
            self.system("no messages to jump to yet");
            return;
        }
        let a = arg.trim().to_ascii_lowercase();
        let target = match a.as_str() {
            "" => {
                self.system("usage: /goto <N | 5m | 1h | top | end>");
                return;
            }
            "top" | "start" | "first" => 1,
            "end" | "bottom" | "last" => {
                self.scroll_offset = 0; // follow newest
                self.system("jumped to the latest output");
                return;
            }
            _ => {
                if let Ok(n) = a.parse::<usize>() {
                    n.clamp(1, count)
                } else if let Some(secs) = parse_duration(&a) {
                    let cutoff = chrono::Local::now() - chrono::Duration::seconds(secs);
                    // First message at/after the cutoff; if all are older, the
                    // newest one is closest to "that long ago".
                    self.first_message_since(cutoff).unwrap_or(count)
                } else {
                    self.system("usage: /goto <N | 5m | 1h | top | end>");
                    return;
                }
            }
        };
        self.pending_goto = Some(target);
        self.system(format!("jumped to message #{target}"));
    }
    /// `/find <text>` — search the transcript and jump to the next match
    /// (case-insensitive). No arg cycles to the next match of the current query;
    /// `/find clear` (or `off`/`discard`) drops the search + highlight.
    fn find_cmd(&mut self, arg: &str) {
        // Clear the active search + highlight.
        if matches!(
            arg.trim().to_ascii_lowercase().as_str(),
            "clear" | "off" | "discard"
        ) {
            if self.find_query.is_some() {
                self.find_query = None;
                self.find_pos = 0;
                self.system("search cleared");
            } else {
                self.system("no active search");
            }
            return;
        }
        let arg = arg.trim();
        if arg.is_empty() {
            if self.find_query.is_none() {
                self.system("usage: /find <text>");
                return;
            }
        } else {
            // A new query restarts cycling from the top.
            if self.find_query.as_deref() != Some(arg) {
                self.find_pos = 0;
            }
            self.find_query = Some(arg.to_string());
        }
        self.find_cycle(true);
    }
    /// Message numbers (1-based) whose text contains `query` (case-insensitive).
    fn find_hits(&self, query: &str) -> Vec<usize> {
        hrdr_app::find_hits(&self.transcript, query)
    }
    /// Cycle to the next (`forward`) or previous match of the active query,
    /// wrapping around; used by `/find`, `/next`, and `/prev`.
    fn find_cycle(&mut self, forward: bool) {
        let Some(query) = self.find_query.clone() else {
            self.system("no active search — /find <text>");
            return;
        };
        let hits = self.find_hits(&query);
        if hits.is_empty() {
            self.system(format!("no match for {query:?}"));
            return;
        }
        let target = if forward {
            hits.iter()
                .copied()
                .find(|&n| n > self.find_pos)
                .unwrap_or(hits[0])
        } else {
            hits.iter()
                .rev()
                .copied()
                .find(|&n| n < self.find_pos)
                .unwrap_or(*hits.last().unwrap())
        };
        let idx = hits.iter().position(|&n| n == target).unwrap_or(0) + 1;
        self.find_pos = target;
        self.pending_goto = Some(target);
        self.system(format!(
            "match {idx}/{} for {query:?} → message #{target}",
            hits.len()
        ));
    }
    /// Number of user/assistant messages in the transcript.
    fn display_message_count(&self) -> usize {
        hrdr_app::message_count(&self.transcript)
    }
    /// The number of the first user/assistant message sent at/after `cutoff`.
    fn first_message_since(&self, cutoff: chrono::DateTime<chrono::Local>) -> Option<usize> {
        hrdr_app::first_message_since(&self.transcript, &self.entry_times, cutoff)
    }
    /// The text of the Nth (1-based) user/assistant message in the transcript.
    fn nth_message_text(&self, n: usize) -> Option<String> {
        hrdr_app::nth_message_text(&self.transcript, n)
    }
    /// Write `text` to the system clipboard, returning a status line (used by the
    /// shared `/copy` via [`hrdr_app::CommandHost`]).
    pub(super) fn clipboard_status(&mut self, text: &str, label: &str) -> String {
        let res = self
            .clipboard
            .as_mut()
            .map(|cb| cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()));
        match res {
            Some(Ok(())) => format!("copied {label} to clipboard"),
            Some(Err(_)) => "clipboard write failed".to_string(),
            None => "clipboard unavailable".to_string(),
        }
    }
    /// The most recent assistant message text.
    fn last_assistant_text(&self) -> Option<String> {
        self.transcript.iter().rev().find_map(|e| match e {
            Entry::Assistant(s) => Some(s.clone()),
            _ => None,
        })
    }
    /// The most recent fenced code block across assistant messages.
    fn last_code_block(&self) -> Option<String> {
        self.transcript.iter().rev().find_map(|e| match e {
            Entry::Assistant(s) => last_fenced_block(s),
            _ => None,
        })
    }
    /// A plain-text rendering of the conversation for `/copy all`.
    fn transcript_text(&self) -> String {
        hrdr_app::transcript_to_text(&self.transcript)
    }
    /// `/edit <file>` — open a file (relative to the cwd) in `$EDITOR`.
    fn edit_file_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /edit <file>");
            return;
        }
        if self.running {
            self.system("can't /edit while a turn is running");
            return;
        }
        let Some(cwd) = self.with_agent_or_busy(|a| a.cwd()) else {
            return;
        };
        let path = resolve_under(&cwd, arg);
        // Consumed by the run loop (it owns the terminal needed to suspend).
        self.pending_edit = Some(path);
    }
}

/// Keybinding tips appended to the shared `/help` body (TUI-specific).
const HELP_TIPS: &str =
    "Tips: @path attaches a file · Up/Down recalls history · Ctrl+L redraws · Ctrl+C twice quits";

/// The TUI's [`hrdr_app::CommandHost`] — a thin adapter over `App` so the shared
/// slash-command dispatcher can drive it. Commands with a richer TUI rendering
/// stay in [`App::handle_slash`] and never reach here.
struct TuiHost<'a> {
    app: &'a mut super::App,
}

impl hrdr_app::CommandHost for TuiHost<'_> {
    fn info(&mut self, line: String) {
        self.app.system(line);
    }
    fn spawn_line(&self, fut: hrdr_app::LineFuture) {
        let tx = self.app.tx.clone();
        tokio::spawn(async move {
            let line = fut.await;
            if !line.is_empty() {
                let _ = tx.send(TurnMsg::System(line));
            }
        });
    }
    fn agent(&self) -> std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>> {
        self.app.agent.clone()
    }
    fn cwd(&self) -> std::path::PathBuf {
        self.app
            .with_agent(|a| a.cwd())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }
    fn base_url(&self) -> String {
        self.app.base_url.clone()
    }
    fn model(&self) -> String {
        self.app.model.clone()
    }
    fn set_model(&mut self, model: String) {
        self.app.model = model;
    }
    fn show_thinking(&self) -> bool {
        self.app.show_reasoning
    }
    fn set_show_thinking(&mut self, on: bool) {
        self.app.show_reasoning = on;
        self.app
            .persist_setting("show_thinking", hrdr_agent::ConfigValue::Bool(on));
    }
    fn clear_conversation(&mut self) {
        self.app.clear_all();
    }
    fn session_id(&self) -> Option<String> {
        self.app.session_id.clone()
    }
    fn set_session_label(&mut self, name: String) {
        self.app.session_label = Some(name);
    }
    fn autosave(&mut self) {
        self.app.autosave();
    }
    fn resume(&mut self, id: String, session: hrdr_agent::Session) {
        self.app.apply_session(id, session);
    }
    fn copy_to_clipboard(&mut self, text: &str, label: &str) -> String {
        self.app.clipboard_status(text, label)
    }
    fn last_reply(&self) -> Option<String> {
        self.app.last_assistant_text()
    }
    fn transcript_text(&self) -> String {
        self.app.transcript_text()
    }
    fn nth_message_text(&self, n: usize) -> Option<String> {
        self.app.nth_message_text(n)
    }
    fn last_code_block(&self) -> Option<String> {
        // Richer than the default: searches back across assistant messages.
        self.app.last_code_block()
    }
    fn spawn_diff(&self, fut: hrdr_app::LineFuture) {
        // Route a real diff to the colored Entry::Diff rendering; status and
        // error lines stay plain system lines.
        let tx = self.app.tx.clone();
        tokio::spawn(async move {
            let line = fut.await;
            if line.is_empty() {
                return;
            }
            let msg = if line.starts_with("diff ") {
                TurnMsg::Diff(line)
            } else {
                TurnMsg::System(line)
            };
            let _ = tx.send(msg);
        });
    }
    fn supports_command(&self, _cmd: &str) -> bool {
        true // the TUI implements the full registry
    }
    fn is_busy(&self) -> bool {
        self.app.running
    }
    fn send_prompt(&mut self, prompt: String, show_as_user: bool) {
        if show_as_user {
            self.app.spawn_turn(prompt);
        } else {
            self.app.scroll_offset = 0;
            self.app.launch_turn(prompt);
        }
    }
    fn set_input(&mut self, text: String) {
        self.app.editor.set_content(&text);
    }
    fn prepend_input(&mut self, text: String) {
        let existing = self.app.editor.content();
        self.app.editor.set_content(&format!("{text}{existing}"));
    }
    fn insert_input(&mut self, text: String) {
        self.app.editor.paste(&text);
    }
    fn read_clipboard(&self) -> Option<String> {
        let bytes = self
            .app
            .clipboard
            .as_ref()
            .and_then(|cb| cb.get(Selection::Clipboard, MimeType::Text).ok())?;
        Some(String::from_utf8_lossy(&bytes).to_string())
    }
    fn set_tool_expansion(&mut self, mode: hrdr_app::ExpandMode) -> String {
        self.app.apply_tool_expansion(mode)
    }
    fn rewind_last_turn(&mut self) -> Option<String> {
        self.app.rewind_last_turn()
    }
    fn compact(&mut self, instructions: Option<String>) {
        self.app.system("compacting conversation…");
        self.app.spawn_compaction(instructions);
    }
    fn persist_setting(&mut self, key: &str, value: hrdr_agent::ConfigValue) {
        // The TUI version also suppresses the config hot-reload it would cause.
        self.app.persist_setting(key, value);
    }
    fn effort(&self) -> Option<String> {
        self.app.effort.clone()
    }
    fn set_effort(&mut self, label: String) {
        self.app.effort = Some(label);
    }
    fn cwd_changed(&mut self, new: &std::path::Path) {
        self.app.dir = hrdr_app::display_dir(new);
        self.app.branch = hrdr_app::git_branch(new);
        self.app.file_index_cwd = None; // rebuild @-completion for the new dir
    }
    fn files_changed(&mut self) {
        self.app.file_index_cwd = None;
    }
    fn timestamp_style(&self) -> hrdr_app::TimestampStyle {
        self.app.timestamp_style
    }
    fn set_timestamp_style(&mut self, style: hrdr_app::TimestampStyle) {
        self.app.timestamp_style = style;
    }
    fn todo_ttl(&self) -> u64 {
        self.app.todo_ttl
    }
    fn statusbar_mode(&self) -> hrdr_app::StatusBarMode {
        self.app.statusbar_mode
    }
    fn set_statusbar_mode(&mut self, mode: hrdr_app::StatusBarMode) {
        self.app.statusbar_mode = mode;
    }
    fn set_theme(&mut self, path: Option<String>) {
        self.app.theme = Theme::load(path.as_deref());
    }
    fn unpersist_setting(&mut self, key: &str) {
        // The TUI version also suppresses the config hot-reload it would cause.
        self.app.unpersist_setting(key);
    }
    fn set_todo_ttl(&mut self, turns: u64) {
        self.app.todo_ttl = turns;
    }
    fn resolve_provider(&self, name: &str) -> Option<hrdr_agent::ResolvedProvider> {
        self.app.cfg.resolve_provider(name)
    }
    fn set_base_url(&mut self, url: String) {
        self.app.base_url = url;
    }
    fn set_context_window(&mut self, tokens: Option<u32>) {
        if tokens.is_some() {
            self.app.context_window = tokens;
        }
    }
    fn help_tips(&self) -> Option<String> {
        Some(HELP_TIPS.to_string())
    }
}

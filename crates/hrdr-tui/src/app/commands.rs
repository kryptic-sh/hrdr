//! Slash-command dispatch and the individual command handlers.

use super::*;
use crate::theme::Theme;
use hrdr_app::{last_fenced_block, resolve_alias, resolve_under};

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
        // `hrdr_app` dispatcher (so every frontend runs one implementation).
        match cmd {
            "edit" => self.edit_file_cmd(arg),
            "reload" => self.reload_cmd(),
            "goto" => self.goto_cmd(arg),
            "find" | "search" => self.find_cmd(arg),
            "next" => self.find_cycle(true),
            "prev" | "previous" => self.find_cycle(false),
            // help, clear, model, tools, copy, diff, rename, thinking,
            // sessions, resume, export → shared dispatcher (TuiHost overrides
            // route /diff to the colored EntryKind::Diff rendering).
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
        if let Ok(mut a) = self.agent.try_lock() {
            a.clear();
        } else {
            // The just-aborted turn still holds the lock until its task drops;
            // clear through an awaited lock.
            let agent = self.agent.clone();
            tokio::spawn(async move {
                agent.lock().await.clear();
            });
        }
        self.clear_transcript();
        // `/clear` starts a new session, so it opens with the banner again.
        self.push_entry(Entry::header());
        self.queue.clear();
        if let Ok(mut q) = self.steering.lock() {
            q.clear();
        }
        if let Ok(mut todos) = self.todos.lock() {
            todos.clear();
        }
        self.todo_turn = 0;
        self.todo_completed_at.clear();
        self.scroll_offset = 0;
        self.max_scroll = 0;
        // Reset the counters, but keep the probed context window: it describes
        // the model/endpoint, not the conversation.
        self.state.usage = hrdr_app::SessionUsage {
            context_window: self.state.usage.context_window,
            ..Default::default()
        };
        self.last_cached_tokens = None;
        self.last_reasoning_tokens = None;
        self.state.id = None; // detach; next message starts a new session
        self.state.name.clear();
        self.find = hrdr_app::FindState::default();
        self.pending_goto = None;
        self.pending_edit = None;
        self.login = None;
        self.expand_tools = false;
    }
    /// Apply an `/expand` mode (shared dispatch parses the arg), returning the
    /// status line. `expand_tools` is the sticky all-on flag; per-entry
    /// expansion lives on the Tool entries.
    pub(super) fn apply_tool_expansion(&mut self, mode: hrdr_app::ExpandMode) -> String {
        match mode {
            hrdr_app::ExpandMode::All => {
                self.expand_tools = true;
                hrdr_app::expand_msg::ALL.to_string()
            }
            hrdr_app::ExpandMode::Off => {
                self.expand_tools = false;
                for e in self.state.transcript.iter_mut() {
                    if let EntryKind::Tool { expanded, .. } = &mut e.kind {
                        *expanded = false;
                    }
                }
                hrdr_app::expand_msg::OFF.to_string()
            }
            hrdr_app::ExpandMode::ToggleLast => {
                // Keep the toggled block's top where the reader is looking; its
                // height is about to change (see `pending_scroll_entry`).
                let idx = self
                    .state
                    .transcript
                    .iter()
                    .rposition(|e| matches!(e.kind, EntryKind::Tool { .. }));
                self.pending_scroll_entry = idx;
                let last = self
                    .state
                    .transcript
                    .iter_mut()
                    .rev()
                    .find_map(|e| match &mut e.kind {
                        EntryKind::Tool { expanded, .. } => Some(expanded),
                        _ => None,
                    });
                match last {
                    Some(expanded) => {
                        *expanded = !*expanded;
                        if *expanded {
                            hrdr_app::expand_msg::LAST_EXPANDED.to_string()
                        } else {
                            hrdr_app::expand_msg::LAST_COLLAPSED.to_string()
                        }
                    }
                    None => hrdr_app::expand_msg::NONE.to_string(),
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
    /// Rewind the last user turn out of the agent history + transcript,
    /// returning the user's text (shared `/undo` and `/retry` core).
    pub(super) fn rewind_last_turn(&mut self) -> Option<String> {
        let text = self
            .agent
            .try_lock()
            .ok()
            .and_then(|mut a| a.rewind_last_user())?;
        if let Some(idx) = self
            .state
            .transcript
            .iter()
            .rposition(|e| matches!(e.kind, EntryKind::User(_)))
        {
            self.truncate_transcript(idx);
        }
        self.scroll_offset = 0;
        Some(text)
    }
    /// `/goto <N | 5m | 1h | top | end>` — scroll the transcript to a message
    /// number, to the message nearest a relative time ago, or to top/bottom
    /// (shared [`hrdr_app::goto_action`] core; only the scrolling is local).
    fn goto_cmd(&mut self, arg: &str) {
        let act = hrdr_app::goto_action(arg, self.display_message_count(), |cutoff| {
            self.first_message_since(cutoff)
        });
        self.apply_find_action(act);
    }
    /// `/find <text>` — search the transcript and jump to the next match
    /// (case-insensitive). No arg cycles to the next match of the current query;
    /// `/find clear` (or `off`/`discard`) drops the search + highlight.
    fn find_cmd(&mut self, arg: &str) {
        let mut st = std::mem::take(&mut self.find);
        let act = st.find(arg, |q| hrdr_app::find_hits(&self.state.transcript, q));
        self.find = st;
        self.apply_find_action(act);
    }
    /// Cycle to the next (`forward`) or previous match of the active query,
    /// wrapping around; used by `/next` and `/prev`.
    fn find_cycle(&mut self, forward: bool) {
        let mut st = std::mem::take(&mut self.find);
        let act = st.cycle(forward, |q| hrdr_app::find_hits(&self.state.transcript, q));
        self.find = st;
        self.apply_find_action(act);
    }
    /// Route a resolved find/goto action to the TUI's scroll primitives.
    fn apply_find_action(&mut self, act: hrdr_app::FindAction) {
        match act {
            hrdr_app::FindAction::Info(line) => self.system(line),
            hrdr_app::FindAction::Jump { msg, line } => {
                self.pending_goto = Some(msg);
                self.system(line);
            }
            hrdr_app::FindAction::Bottom { line } => {
                self.scroll_offset = 0; // follow newest
                self.system(line);
            }
        }
    }
    /// Number of user/assistant messages in the transcript.
    fn display_message_count(&self) -> usize {
        hrdr_app::message_count(&self.state.transcript)
    }
    /// The number of the first user/assistant message sent at/after `cutoff`.
    fn first_message_since(&self, cutoff: chrono::DateTime<chrono::Local>) -> Option<usize> {
        hrdr_app::first_message_since(&self.state.transcript, cutoff)
    }
    /// The text of the Nth (1-based) user/assistant message in the transcript.
    fn nth_message_text(&self, n: usize) -> Option<String> {
        hrdr_app::nth_message_text(&self.state.transcript, n)
    }
    /// Write `text` to the system clipboard, returning a status line (used by the
    /// shared `/copy` via [`hrdr_app::CommandHost`]).
    pub(super) fn clipboard_status(&mut self, text: &str, label: &str) -> String {
        hrdr_app::clipboard_copy_status(&mut self.clipboard, text, label)
    }
    /// The most recent assistant message text.
    fn last_assistant_text(&self) -> Option<String> {
        self.state
            .transcript
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                EntryKind::Assistant(s) => Some(s.clone()),
                _ => None,
            })
    }
    /// The most recent fenced code block across assistant messages.
    fn last_code_block(&self) -> Option<String> {
        self.state
            .transcript
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                EntryKind::Assistant(s) => last_fenced_block(s),
                _ => None,
            })
    }
    /// A plain-text rendering of the conversation for `/copy all`.
    fn transcript_text(&self) -> String {
        hrdr_app::transcript_to_text(&self.state.transcript)
    }
    /// `/edit <file>` — open a file (relative to the cwd) in `$EDITOR`.
    fn edit_file_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /edit <file>");
            return;
        }
        if self.running {
            self.system(hrdr_app::busy_guard("/edit"));
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

/// Tips appended to the shared `/help` body (TUI-specific). The input
/// discipline's own keys are added ahead of these, from
/// [`hrdr_editor::EditorEngine::keybind_hint`], so vim and plain mode each
/// advertise their own.
const HELP_TIPS: &str = "  @path attaches a file · Up/Down recalls history\n       Ctrl+L redraws · Ctrl+C twice quits · click a tool block to expand it\n       click a sub-agent to jump to its task call\n       PgUp/PgDn scrolls · Home/End jumps · END follows new output";

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
    fn line_poster(&self) -> Box<dyn Fn(hrdr_app::LineKind, String) + Send> {
        let tx = self.app.tx.clone();
        Box::new(move |kind, line| {
            let msg = match kind {
                hrdr_app::LineKind::Diff => TurnMsg::Diff(line),
                hrdr_app::LineKind::System => TurnMsg::System(line),
            };
            let _ = tx.send(msg);
        })
    }
    fn context_window_poster(&self) -> Box<dyn Fn(u32) + Send> {
        let tx = self.app.tx.clone();
        Box::new(move |tokens| {
            let _ = tx.send(TurnMsg::ContextWindow(tokens));
        })
    }
    fn agent(&self) -> std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>> {
        self.app.agent.clone()
    }
    fn cwd(&self) -> std::path::PathBuf {
        hrdr_app::agent_cwd(&self.app.agent)
    }
    fn base_url(&self) -> String {
        self.app.state.base_url.clone()
    }
    fn model(&self) -> String {
        self.app.state.model.clone()
    }
    fn set_model(&mut self, model: String) {
        self.app.state.model = model;
    }
    fn provider(&self) -> Option<String> {
        self.app.state.provider.clone()
    }
    fn set_provider(&mut self, name: String) {
        self.app.state.provider = Some(name);
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
        self.app.state.id.clone()
    }
    fn set_session_label(&mut self, name: String) {
        self.app.state.name = name;
    }
    fn autosave(&mut self) {
        self.app.autosave();
    }
    fn resume(&mut self, id: String, session: hrdr_app::Session) {
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
        hrdr_app::clipboard_read_text(&self.app.clipboard)
    }
    fn set_tool_expansion(&mut self, mode: hrdr_app::ExpandMode) -> String {
        self.app.apply_tool_expansion(mode)
    }
    fn rewind_last_turn(&mut self) -> Option<String> {
        self.app.rewind_last_turn()
    }
    fn start_compaction(&mut self, instructions: Option<String>) {
        self.app.spawn_compaction(instructions);
    }
    fn mark_init_turn(&mut self) {
        self.app.pending_init = true;
    }
    fn persist_setting(&mut self, key: &str, value: hrdr_agent::ConfigValue) {
        // The TUI version also suppresses the config hot-reload it would cause.
        self.app.persist_setting(key, value);
    }
    fn effort(&self) -> Option<String> {
        self.app.effort.clone()
    }
    fn session_label(&self) -> Option<String> {
        Some(self.app.state.name.clone()).filter(|n| !n.is_empty())
    }
    fn context_usage(&self) -> Option<(u32, u32)> {
        self.app.state.usage.last()
    }
    fn context_window(&self) -> Option<u32> {
        self.app.state.usage.context_window
    }
    fn session_tokens(&self) -> (usize, usize) {
        (
            self.app.state.usage.tokens_in,
            self.app.state.usage.tokens_out,
        )
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
        self.app.state.base_url = url;
    }
    fn set_context_window(&mut self, tokens: Option<u32>) {
        if tokens.is_some() {
            self.app.state.usage.context_window = tokens;
        }
    }
    fn begin_login(&mut self) {
        let wizard = hrdr_app::LoginWizard::start(self);
        self.app.login = Some(wizard);
    }
    fn begin_model_selector(&mut self) {
        let choices = hrdr_agent::model_choices(&self.app.cfg, self.app.state.provider.as_deref());
        if choices.is_empty() {
            self.info(
                "no models to choose from yet — configure a provider (or run a turn so the \
                 models.dev catalog is cached), then try /model again"
                    .to_string(),
            );
            return;
        }
        self.app.model_selector = Some(super::ModelSelector::new(choices));
    }
    fn help_tips(&self) -> Option<String> {
        // The footer no longer repeats these, so `/help` is where they live.
        Some(format!(
            "Keys:\n  {}\n{HELP_TIPS}",
            self.app.editor.keybind_hint()
        ))
    }
}

impl super::App {
    /// Route one key to the open `/model` selector: Esc/Ctrl+C closes it,
    /// Up/Down move the highlight, Enter applies the highlighted model (switching
    /// provider + model), and any other character edits the fuzzy filter. Caller
    /// checks `self.model_selector.is_some()` first.
    pub(super) fn model_selector_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.model_selector = None,
            KeyCode::Char('c') if ctrl => self.model_selector = None,
            KeyCode::Up => {
                if let Some(s) = &mut self.model_selector {
                    s.up();
                }
            }
            KeyCode::Down => {
                if let Some(s) = &mut self.model_selector {
                    s.down();
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = &mut self.model_selector {
                    s.backspace();
                }
            }
            KeyCode::Enter => {
                let choice = self
                    .model_selector
                    .as_ref()
                    .and_then(|s| s.current())
                    .cloned();
                self.model_selector = None;
                if let Some(c) = choice {
                    // Scope the host borrow so the confirmation line can be pushed
                    // afterwards.
                    let line = {
                        let mut host = TuiHost { app: self };
                        match hrdr_app::apply_choice(&mut host, &c.provider, c.model.clone()) {
                            Ok(()) => {
                                format!("model → {} · {}", c.model_label, c.provider_label)
                            }
                            Err(e) => e,
                        }
                    };
                    self.system(line);
                }
            }
            KeyCode::Char(ch) if !ctrl => {
                if let Some(s) = &mut self.model_selector {
                    s.push_char(ch);
                }
            }
            _ => {}
        }
    }

    /// Feed one submitted line to the active `/login` wizard, dropping the modal
    /// slot when it finishes. Caller checks `self.login.is_some()` first.
    pub(super) fn login_feed(&mut self, line: &str) {
        let Some(mut wizard) = self.login.take() else {
            return;
        };
        let done = {
            let mut host = TuiHost { app: self };
            wizard.step(line, &mut host)
        };
        if !done {
            self.login = Some(wizard);
        }
    }
}

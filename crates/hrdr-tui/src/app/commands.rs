//! Slash-command dispatch and the individual command handlers.

use super::*;
use crate::theme::Theme;
use hrdr_app::{CommandHost, last_fenced_block, resolve_alias, resolve_under};

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
        if self.running() {
            self.cancel_turn();
        }
        // `Agent::clear` re-reads `AGENTS.md` from disk: a *new* conversation starts
        // from what the project says now. (A *running* one is never re-seeded — the
        // agent that edited the file already has the change in its context.) Say so
        // when it actually changed.
        let reloaded = if let Ok(mut a) = self.agent.try_lock() {
            a.clear();
            a.project_docs_changed()
        } else {
            // The just-aborted turn still holds the lock until its task drops;
            // clear through an awaited lock.
            let agent = self.agent.clone();
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let mut a = agent.lock().await;
                a.clear();
                if a.project_docs_changed() {
                    let _ = tx.send(TurnMsg::System(
                        hrdr_app::PROJECT_DOCS_RELOADED_MSG.to_string(),
                    ));
                }
            });
            false
        };
        self.clear_transcript();
        // `/clear` starts a new session, so it opens with the banner again.
        self.push_entry(Entry::header());
        if reloaded {
            self.push_entry(Entry::notice(hrdr_app::PROJECT_DOCS_RELOADED_MSG));
        }
        self.live_subagents.clear_pending(hrdr_agent::MAIN_KEY);
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
        let window = self.state().usage.context_window;
        self.state_mut().usage = hrdr_app::SessionUsage {
            context_window: window,
            ..Default::default()
        };
        // The registry is what the main pane is rebuilt from, so the reset has to
        // land there too (`Agent::clear` zeroes the agent's own cost counter).
        self.publish_main_agent();
        self.state_mut().id = None; // detach; next message starts a new session
        // Detach the sub-agent transcript dir with it — otherwise a `task`
        // spawned early in the next session (before its first autosave assigns
        // an id) would resolve this now-abandoned session's dir and misfile its
        // transcript there. Cleared to `None` = not persisted until the new id
        // lands, matching the documented pre-first-save behavior.
        if let Ok(mut cell) = self.subagent_dir.lock() {
            *cell = None;
        }
        self.state_mut().name.clear();
        self.find = hrdr_app::FindState::default();
        self.pending_goto = None;
        self.pending_edit = None;
        self.login_modal = None;
        self.skill_selector = None;
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
                for e in self.panes.active_transcript_mut().iter_mut() {
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
                    .panes
                    .active_transcript()
                    .iter()
                    .rposition(|e| matches!(e.kind, EntryKind::Tool { .. }));
                self.pending_scroll_entry = idx;
                let last = self
                    .panes
                    .active_transcript_mut()
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
    /// `/reload` — re-read config (and rediscover skills), applying the runtime bits
    /// that can change live; keeps the current settings if the config is invalid.
    ///
    /// It does **not** re-seed `AGENTS.md` into a running conversation. Project docs
    /// are part of the system prompt this conversation was started with; replacing
    /// them underneath it means the model has been told two different things about
    /// the project in one context. A changed `AGENTS.md` is picked up by the next
    /// conversation (`/new`).
    fn reload_cmd(&mut self) {
        self.apply_config_reload(true);
        self.skills = hrdr_app::discover_skills(&std::path::PathBuf::from(self.current_cwd()));
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
            .panes
            .main()
            .transcript()
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
        let act = st.find(arg, |q| {
            hrdr_app::find_hits(self.panes.active_transcript(), q)
        });
        self.find = st;
        self.apply_find_action(act);
    }
    /// Cycle to the next (`forward`) or previous match of the active query,
    /// wrapping around; used by `/next` and `/prev`.
    fn find_cycle(&mut self, forward: bool) {
        let mut st = std::mem::take(&mut self.find);
        let act = st.cycle(forward, |q| {
            hrdr_app::find_hits(self.panes.active_transcript(), q)
        });
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
    // These read *the conversation on screen*, like every other command: `/copy
    // msg 3` in a sub-agent's view means that agent's third message.
    /// Number of user/assistant messages in the transcript.
    fn display_message_count(&self) -> usize {
        hrdr_app::message_count(self.panes.active_transcript())
    }
    /// The number of the first user/assistant message sent at/after `cutoff`.
    fn first_message_since(&self, cutoff: chrono::DateTime<chrono::Local>) -> Option<usize> {
        hrdr_app::first_message_since(self.panes.active_transcript(), cutoff)
    }
    /// The text of the Nth (1-based) user/assistant message in the transcript.
    fn nth_message_text(&self, n: usize) -> Option<String> {
        hrdr_app::nth_message_text(self.panes.active_transcript(), n)
    }
    /// Write `text` to the system clipboard, returning a status line (used by the
    /// shared `/copy` via [`hrdr_app::CommandHost`]).
    pub(super) fn clipboard_status(&mut self, text: &str, label: &str) -> String {
        hrdr_app::clipboard_copy_status(&mut self.clipboard, text, label)
    }
    /// The most recent assistant message text.
    fn last_assistant_text(&self) -> Option<String> {
        self.panes
            .active_transcript()
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                EntryKind::Assistant(s) => Some(s.clone()),
                _ => None,
            })
    }
    /// The most recent fenced code block across assistant messages.
    fn last_code_block(&self) -> Option<String> {
        self.panes
            .active_transcript()
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                EntryKind::Assistant(s) => last_fenced_block(s),
                _ => None,
            })
    }
    /// A plain-text rendering of the conversation for `/copy all`.
    fn transcript_text(&self) -> String {
        hrdr_app::transcript_to_text(self.panes.active_transcript())
    }
    /// `/edit <file>` — open a file (relative to the cwd) in `$EDITOR`.
    fn edit_file_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /edit <file>");
            return;
        }
        if self.running() {
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
pub(super) struct TuiHost<'a> {
    pub(super) app: &'a mut super::App,
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
        // Bind the pane *now*, when the command is issued: the probe lands later,
        // by which time the user may be looking at a different agent — and the
        // window belongs to the agent that was switched, not to whatever is on
        // screen when the answer arrives.
        let id = self.app.panes.active();
        Box::new(move |tokens| {
            let _ = tx.send(TurnMsg::ContextWindow(id, tokens));
        })
    }
    fn agent(&self) -> std::sync::Arc<tokio::sync::Mutex<hrdr_agent::Agent>> {
        // Commands act on the agent you are looking at — the same rule as the
        // input box. A sub-agent is an agent: `/compact`, `/tools`, `/prompt`,
        // `/status`, `/doctor` and friends all mean *this conversation*.
        self.app.active_agent()
    }

    fn cwd(&self) -> std::path::PathBuf {
        hrdr_app::agent_cwd(&self.app.agent)
    }
    // Chrome — model, provider, endpoint, context window — belongs to whichever
    // agent is on screen, and is read back from it. `/model` on a sub-agent's view
    // switches *that* agent's model, and the status bar (which reads the active
    // pane) shows it, because they are the same piece of state.
    fn base_url(&self) -> String {
        self.app.panes.active_pane().state.base_url.clone()
    }
    fn model_ref(&self) -> hrdr_agent::ModelRef {
        self.app.active_model_ref()
    }
    fn set_model_ref(&mut self, reference: hrdr_agent::ModelRef) {
        self.app.set_active_model_ref(reference);
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
        self.app.state().id.clone()
    }
    fn set_session_label(&mut self, name: String) {
        self.app.state_mut().name = name;
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
        self.app.running() || self.app.compacting()
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
    fn persist_setting(&mut self, key: &str, value: hrdr_agent::ConfigValue) {
        // The TUI version also suppresses the config hot-reload it would cause.
        self.app.persist_setting(key, value);
    }
    fn effort(&self) -> Option<String> {
        self.app.panes.active_pane().effort.clone()
    }
    fn session_label(&self) -> Option<String> {
        Some(self.app.state().name.clone()).filter(|n| !n.is_empty())
    }
    // Usage reads the conversation on screen too: `/status` and `/cost` in a
    // sub-agent's view report what *that* agent has used and cost.
    fn context_usage(&self) -> Option<(u32, u32)> {
        self.app.panes.active_pane().state.usage.last()
    }
    fn context_window(&self) -> Option<u32> {
        self.app.panes.active_pane().state.usage.context_window
    }
    fn session_tokens(&self) -> (usize, usize) {
        let u = &self.app.panes.active_pane().state.usage;
        (u.tokens_in, u.tokens_out)
    }
    fn session_cost(&self) -> f64 {
        self.app.panes.active_pane().state.usage.cost_usd
    }
    fn set_effort(&mut self, label: String) {
        // Effort is the agent's; it publishes the change back into the chrome.
        let agent = self.app.active_agent();
        tokio::spawn(async move {
            agent.lock().await.set_effort(Some(label));
        });
    }
    fn cwd_changed(&mut self, new: &std::path::Path) {
        self.app.dir = hrdr_app::display_dir(new);
        self.app.branch = hrdr_app::git_branch(new);
        self.app.file_index_cwd = None; // rebuild @-completion for the new dir
        self.app.skills = hrdr_app::discover_skills(new);
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
        self.app.set_active_base_url(url);
    }
    fn set_context_window(&mut self, tokens: Option<u32>) {
        if tokens.is_some() {
            self.app.set_active_context_window(tokens);
        }
    }
    fn begin_login(&mut self) {
        self.app.login_modal = Some(super::LoginModal::Providers(
            super::login_provider_selector(hrdr_app::login_provider_choices()),
        ));
    }
    fn begin_skill_selector(&mut self) {
        let skills = hrdr_app::discover_skills(&std::path::PathBuf::from(self.app.current_cwd()));
        if skills.is_empty() {
            self.info(
                "no skills yet — put Markdown prompt templates in .hrdr/skills/ (or \
                 .claude/commands/, ~/.config/hrdr/skills/), then invoke one with \
                 :name [arguments]"
                    .to_string(),
            );
            return;
        }
        self.app.skill_selector = Some(super::skill_selector(skills));
    }
    fn begin_model_selector(&mut self) {
        let choices =
            hrdr_agent::model_choices(&self.app.cfg, self.app.state().provider.as_deref());
        // A built-in ChatGPT login contributes rows asynchronously (its models
        // aren't in the sync catalog), so the picker may open even when the sync
        // list is empty — the async load fills it. Gated on the TRUSTED built-in
        // (a custom `chatgpt` shadow resolves to Custom and is left untouched).
        let chatgpt_ready = self.app.cfg.resolve_provider("chatgpt").map(|p| p.kind)
            == Some(hrdr_agent::ResolvedProviderKind::ChatGptOAuth)
            && hrdr_agent::has_oauth_credentials(
                hrdr_agent::ResolvedProviderKind::ChatGptOAuth,
                "chatgpt",
            );
        if choices.is_empty() && !chatgpt_ready {
            self.info(
                "no models to choose from yet — configure a provider (or run a turn so the \
                 models.dev catalog is cached), then try /model again"
                    .to_string(),
            );
            return;
        }
        // A fresh generation for this picker session; a prior load's late result
        // is then rejected.
        self.app.model_gen = self.app.model_gen.wrapping_add(1);
        self.app.model_source = None;
        self.app.model_selector = Some(super::model_selector(choices));
        self.app.spawn_model_catalog_load(false);
    }
    /// The picker, restricted to one provider's models — what `/login <provider>`
    /// opens when the provider declares no model and none was ever used on it.
    /// Naming a provider is a question ("which of its models?"), and this is the
    /// UI asking it, rather than reporting an error the user can't act on.
    fn begin_model_selector_for(&mut self, provider: &str) {
        let name = hrdr_agent::ProviderName::new(provider);
        let mut choices = hrdr_agent::model_choices(&self.app.cfg, Some(name.as_str()));
        choices.retain(|c| hrdr_agent::ProviderName::new(&c.provider) == name);
        // No rows for it (a provider the catalog doesn't cover, before its first
        // turn caches one) → the unfiltered picker is still better than nothing.
        if choices.is_empty() {
            self.begin_model_selector();
            return;
        }
        self.app.model_gen = self.app.model_gen.wrapping_add(1);
        self.app.model_source = None;
        self.app.model_selector = Some(super::model_selector(choices));
        self.app.spawn_model_catalog_load(true);
    }
    fn begin_session_selector(&mut self) {
        // Every directory's sessions, newest first — the cwd column tells them
        // apart, and the fuzzy filter narrows by it too.
        let sessions = hrdr_app::list_sessions();
        if sessions.is_empty() {
            self.info(format!(
                "no saved sessions yet in {}",
                hrdr_app::sessions_dir().display()
            ));
            return;
        }
        self.app.session_selector = Some(super::session_selector(sessions));
    }
    fn begin_effort_selector(&mut self) {
        let choices = hrdr_app::effort_choices(
            self.app.state().provider.as_deref(),
            &self.app.state().model,
        );
        self.app.effort_selector = Some(super::effort_selector(choices));
    }
    fn begin_theme_selector(&mut self) {
        // Remember the theme in force for Esc / a filter that matches nothing
        // (the picker live-previews the highlighted row).
        self.app.theme_original = Some(self.app.theme.clone());
        self.app.theme_selector = Some(super::theme_selector(hrdr_app::theme_choices()));
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
            KeyCode::Esc => self.close_model_selector(),
            KeyCode::Char('c') if ctrl => self.close_model_selector(),
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
            // Enter switches for this session; Ctrl+D also persists the pick as
            // the config default (provider + model), so it sticks next launch.
            KeyCode::Enter => self.apply_selected_model(false),
            KeyCode::Char('d') if ctrl => self.apply_selected_model(true),
            KeyCode::Char(ch) if !ctrl => {
                if let Some(s) = &mut self.model_selector {
                    s.push_char(ch);
                }
            }
            _ => {}
        }
    }

    /// Apply the highlighted model from the `/model` selector and close it. When
    /// `set_default` is true, also persist the pick as the config default
    /// (top-level `provider` + `model`), so it survives a restart.
    fn apply_selected_model(&mut self, set_default: bool) {
        let Some(c) = self
            .model_selector
            .as_ref()
            .and_then(|s| s.current())
            .cloned()
        else {
            self.close_model_selector();
            return;
        };
        self.close_model_selector();
        if set_default {
            self.persist_setting("provider", hrdr_agent::ConfigValue::Str(&c.provider));
            self.persist_setting("model", hrdr_agent::ConfigValue::Str(&c.model));
        }
        // Scope the host borrow so the confirmation line can be pushed after.
        let line = {
            let mut host = TuiHost { app: self };
            match hrdr_app::apply_choice(&mut host, &c.provider, c.model.clone(), c.context_window)
            {
                Ok(()) => {
                    // Bump the selection count (selector ordering).
                    hrdr_agent::record_model_use(&c.provider, &c.model);
                    let what = if set_default {
                        "default set"
                    } else {
                        "model →"
                    };
                    format!("{what} {} · {}", c.model_label, c.provider_label)
                }
                // A turn is running: the live switch is rejected, but a default
                // written to config still took effect for next launch.
                Err(e) if set_default => {
                    format!(
                        "default set: {} · {} — {e}",
                        c.model_label, c.provider_label
                    )
                }
                Err(e) => e,
            }
        };
        self.system(line);
    }

    /// Route one key to the open `/resume` session picker: Esc/Ctrl+C closes
    /// it, Up/Down move the highlight, Enter resumes the highlighted session,
    /// and any other character edits the fuzzy filter. Caller checks
    /// `self.session_selector.is_some()` first.
    pub(super) fn session_selector_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.session_selector = None,
            KeyCode::Char('c') if ctrl => self.session_selector = None,
            KeyCode::Up => {
                if let Some(s) = &mut self.session_selector {
                    s.up();
                }
            }
            KeyCode::Down => {
                if let Some(s) = &mut self.session_selector {
                    s.down();
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = &mut self.session_selector {
                    s.backspace();
                }
            }
            KeyCode::Enter => self.apply_selected_session(),
            KeyCode::Char(ch) if !ctrl => {
                if let Some(s) = &mut self.session_selector {
                    s.push_char(ch);
                }
            }
            _ => {}
        }
    }

    /// Resume the highlighted session from the `/resume` picker and close it.
    /// `apply_session` re-checks the busy guard, so a mid-turn Enter is
    /// rejected with the shared message rather than corrupting the swap.
    fn apply_selected_session(&mut self) {
        let Some(m) = self
            .session_selector
            .as_ref()
            .and_then(|s| s.current())
            .cloned()
        else {
            self.session_selector = None;
            return;
        };
        self.session_selector = None;
        match hrdr_app::Session::load_path(&m.path) {
            Ok(session) => self.apply_session(m.id, session),
            Err(e) => self.system(format!("can't load session {}: {e}", m.id)),
        }
    }

    /// Route one key to the open `/theme` picker: Esc/Ctrl+C closes it and
    /// restores the original theme, Up/Down move the highlight (live-previewing
    /// the highlighted theme), Enter applies + persists it, and any other
    /// character edits the fuzzy filter. Caller checks
    /// `self.theme_selector.is_some()` first.
    pub(super) fn theme_selector_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.close_theme_selector(),
            KeyCode::Char('c') if ctrl => self.close_theme_selector(),
            KeyCode::Up => {
                if let Some(s) = &mut self.theme_selector {
                    s.up();
                }
                self.preview_selected_theme();
            }
            KeyCode::Down => {
                if let Some(s) = &mut self.theme_selector {
                    s.down();
                }
                self.preview_selected_theme();
            }
            KeyCode::Backspace => {
                if let Some(s) = &mut self.theme_selector {
                    s.backspace();
                }
                self.preview_selected_theme();
            }
            KeyCode::Enter => self.apply_selected_theme(),
            KeyCode::Char(ch) if !ctrl => {
                if let Some(s) = &mut self.theme_selector {
                    s.push_char(ch);
                }
                self.preview_selected_theme();
            }
            _ => {}
        }
    }

    /// Route one key to the open `/skills` picker: Esc/Ctrl+C closes it,
    /// Up/Down move the highlight, Enter inserts `:name ` into the input, and
    /// any other character edits the fuzzy filter. Caller checks
    /// `self.skill_selector.is_some()` first.
    pub(super) fn skill_selector_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.skill_selector = None,
            KeyCode::Char('c') if ctrl => self.skill_selector = None,
            KeyCode::Up => {
                if let Some(s) = &mut self.skill_selector {
                    s.up();
                }
            }
            KeyCode::Down => {
                if let Some(s) = &mut self.skill_selector {
                    s.down();
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = &mut self.skill_selector {
                    s.backspace();
                }
            }
            // Enter inserts the invocation and hands the cursor back — the
            // user finishes the arguments and submits like any message.
            KeyCode::Enter => {
                let chosen = self
                    .skill_selector
                    .as_ref()
                    .and_then(|s| s.current())
                    .map(|sk| sk.name.clone());
                self.skill_selector = None;
                if let Some(name) = chosen {
                    self.editor.set_content(&format!(":{name} "));
                }
            }
            KeyCode::Char(ch) if !ctrl => {
                if let Some(s) = &mut self.skill_selector {
                    s.push_char(ch);
                }
            }
            _ => {}
        }
    }

    /// Route one key to the open `/login` modal. Provider phase: a picker
    /// (Esc cancels, Enter picks — OAuth/keyless finish immediately, a
    /// key-based provider advances to the key phase). Key phase: a masked
    /// input (chars/paste append, Enter saves + switches, Esc cancels).
    /// Caller checks `self.login_modal.is_some()` first.
    pub(super) fn login_modal_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.login_modal {
            Some(super::LoginModal::Providers(sel)) => match key.code {
                KeyCode::Esc => self.login_modal = None,
                KeyCode::Char('c') if ctrl => self.login_modal = None,
                KeyCode::Up => sel.up(),
                KeyCode::Down => sel.down(),
                KeyCode::Backspace => sel.backspace(),
                KeyCode::Enter => {
                    let chosen = sel.current().cloned();
                    let Some(c) = chosen else {
                        self.login_modal = None;
                        return;
                    };
                    // Route by resolved trust kind (not spelling): a browser
                    // login enters the TUI's typed pending state; keyless/key go
                    // through the shared pick.
                    let route = {
                        let host = TuiHost { app: self };
                        host.resolve_provider(&c.name)
                            .map(|p| hrdr_app::login_route(&c.name, &p))
                    };
                    if route == Some(hrdr_app::LoginRoute::Browser) {
                        self.start_browser_login(&c.name, c.label.clone());
                        return;
                    }
                    let pick = {
                        let mut host = TuiHost { app: self };
                        hrdr_app::login_pick_provider(&c.name, &mut host)
                    };
                    self.login_modal = match pick {
                        hrdr_app::LoginPick::Done => None,
                        hrdr_app::LoginPick::NeedsKey { name } => {
                            let warning = hrdr_app::login_key_warning(&name);
                            Some(super::LoginModal::Key {
                                name,
                                label: c.label,
                                warning,
                                input: String::new(),
                            })
                        }
                    };
                }
                KeyCode::Char(ch) if !ctrl => sel.push_char(ch),
                _ => {}
            },
            // Browser login in flight: Esc / Ctrl+C abandons it (the in-flight
            // task's late result is rejected by login-id mismatch).
            Some(super::LoginModal::Authorizing { .. }) => match key.code {
                KeyCode::Esc => self.cancel_authorizing(),
                KeyCode::Char('c') if ctrl => self.cancel_authorizing(),
                _ => {}
            },
            // The final provider-switch transaction — not interruptible.
            Some(super::LoginModal::Switching { .. }) => {}
            Some(super::LoginModal::Key { name, input, .. }) => match key.code {
                KeyCode::Esc => {
                    self.login_modal = None;
                    self.system("login cancelled.".to_string());
                }
                KeyCode::Char('c') if ctrl => {
                    self.login_modal = None;
                    self.system("login cancelled.".to_string());
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Enter => {
                    if input.is_empty() {
                        return;
                    }
                    let (name, key_text) = (name.clone(), input.clone());
                    self.login_modal = None;
                    let mut host = TuiHost { app: self };
                    hrdr_app::login_enter_key(&name, &key_text, &mut host);
                }
                KeyCode::Char(ch) if !ctrl => input.push(ch),
                _ => {}
            },
            None => {}
        }
    }

    /// Launch a browser OAuth login into the typed `Authorizing` pending state.
    /// Bumps `login_id` so any prior in-flight login's late result is ignored,
    /// opens the browser via the shared worker, and spawns the exchange/save
    /// future — its outcome arrives as [`TurnMsg::BrowserLogin`].
    fn start_browser_login(&mut self, name: &str, label: String) {
        // One browser login at a time — reject a duplicate defensively (the
        // login-id mechanism is the real guard for a late result).
        if matches!(
            self.login_modal,
            Some(super::LoginModal::Authorizing { .. }) | Some(super::LoginModal::Switching { .. })
        ) {
            self.system("a login is already in progress.".to_string());
            return;
        }
        self.next_login_id += 1;
        let id = self.next_login_id;
        let start = {
            let mut host = TuiHost { app: self };
            hrdr_app::browser_login_start(name, id, &mut host)
        };
        let Some(start) = start else {
            return;
        };
        let provider = start.provider.clone();
        let tx = self.tx.clone();
        let fut = start.future;
        self.browser_login_task = Some(tokio::spawn(async move {
            let outcome = fut.await;
            let _ = tx.send(super::TurnMsg::BrowserLogin(outcome));
        }));
        self.login_modal = Some(super::LoginModal::Authorizing {
            login_id: id,
            provider,
            label,
        });
    }

    /// Abandon an in-flight browser login. The spawned task keeps running but its
    /// [`TurnMsg::BrowserLogin`] will mismatch (no `Authorizing` modal) and be
    /// dropped.
    fn cancel_authorizing(&mut self) {
        // Abort the in-flight task: dropping its future closes the callback
        // listener (freeing the localhost port immediately for a retry) and
        // stops the flow before it can save tokens for an abandoned login.
        if let Some(task) = self.browser_login_task.take() {
            task.abort();
        }
        self.login_modal = None;
        self.system("login cancelled.".to_string());
    }

    /// Handle a finished browser login. Ignores a stale/duplicate result (no
    /// matching `Authorizing` login_id). On success, runs the non-cancellable
    /// switch transaction: persist the default provider, live-switch, report.
    pub(super) fn on_browser_login(&mut self, outcome: hrdr_app::BrowserLoginOutcome) {
        let label = match &self.login_modal {
            Some(super::LoginModal::Authorizing {
                login_id,
                provider,
                label,
            }) if *login_id == outcome.login_id && *provider == outcome.provider => label.clone(),
            // No matching pending login — a stale or cancelled login's late
            // result. Drop it silently.
            _ => return,
        };
        // This login's task has resolved.
        self.browser_login_task = None;
        if !outcome.token_saved {
            self.login_modal = None;
            self.system(format!(
                "{} login failed: {}",
                outcome.provider,
                outcome.error.as_deref().unwrap_or("unknown error")
            ));
            return;
        }
        // Enter the non-interruptible switch transaction.
        self.login_modal = Some(super::LoginModal::Switching { label });
        {
            let mut host = TuiHost { app: self };
            host.persist_setting("provider", hrdr_agent::ConfigValue::Str(&outcome.provider));
            match hrdr_app::apply_provider_or_pick(&mut host, &outcome.provider) {
                Ok(p) => host.info(format!(
                    "✓ signed in to {} ({}). Switched — loading models…",
                    outcome.provider, p.base_url
                )),
                // `NeedsModel` opened the picker on this provider's models: the sign-in
                // worked, and picking a model is the next step, not an error.
                Err(hrdr_app::ProviderSwitchError::NeedsModel { .. }) => host.info(format!(
                    "✓ signed in to {} — pick a model to use it.",
                    outcome.provider
                )),
                Err(e) => host.info(format!(
                    "signed in to {}, but the switch failed: {e}",
                    outcome.provider
                )),
            }
        }
        // Trigger a forced ChatGPT catalog refresh + open the picker (Task 5).
        self.refresh_models_after_login(&outcome.provider);
        self.login_modal = None;
    }

    /// After a successful login, force-refresh the catalog and open the model
    /// picker so the entitled models appear without a restart. Login-forced, so
    /// it may open a closed picker (a plain `/model` load never reopens one).
    fn refresh_models_after_login(&mut self, provider: &str) {
        if provider != "chatgpt" {
            return;
        }
        let choices = hrdr_agent::model_choices(&self.cfg, self.state().provider.as_deref());
        self.model_gen = self.model_gen.wrapping_add(1);
        self.model_source = None;
        self.model_selector = Some(super::model_selector(choices));
        self.spawn_model_catalog_load(true);
    }

    /// Spawn an authenticated ChatGPT catalog load without blocking the UI. Only
    /// runs when a built-in ChatGPT login is set up. Captures the current
    /// generation; the result ([`TurnMsg::ModelCatalog`]) is applied only if the
    /// generation still matches when it lands.
    pub(super) fn spawn_model_catalog_load(&mut self, force: bool) {
        use hrdr_agent::{CHATGPT_CODEX_BASE_URL, CatalogSource, ResolvedProviderKind};
        // Only when the built-in ChatGPT resolves to trusted OAuth (a custom
        // `[providers.chatgpt]` shadow resolves to Custom — leave its rows alone)
        // AND a login is set up.
        if self.cfg.resolve_provider("chatgpt").map(|p| p.kind)
            != Some(ResolvedProviderKind::ChatGptOAuth)
            || !hrdr_agent::has_oauth_credentials(ResolvedProviderKind::ChatGptOAuth, "chatgpt")
        {
            return;
        }
        self.model_loading = true;
        let generation = self.model_gen;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let access = hrdr_agent::coordinated_oauth_access(
                ResolvedProviderKind::ChatGptOAuth,
                CHATGPT_CODEX_BASE_URL,
            )
            .await;
            let msg = match access {
                Ok(access) => {
                    let r = hrdr_agent::chatgpt_model_catalog(&access, force).await;
                    TurnMsg::ModelCatalog {
                        generation,
                        models: r.models,
                        source: r.source,
                        warning: r.warning,
                    }
                }
                // Couldn't obtain a token (revoked/expired refresh, no creds):
                // leave the cached rows in place, but SAY so. The picker opened
                // because credentials looked present, so a silent empty result
                // reads as "you have no models" instead of "you are signed out".
                // The error itself is not shown — it can name the token store.
                Err(_) => TurnMsg::ModelCatalog {
                    generation,
                    models: Vec::new(),
                    source: CatalogSource::BuiltInFallback,
                    warning: Some(
                        "⚠ ChatGPT credentials could not be refreshed — run /login. \
                         Showing built-in ChatGPT models."
                            .to_string(),
                    ),
                },
            };
            let _ = tx.send(msg);
        });
    }

    /// Apply a finished catalog load. Drops a stale generation (picker closed /
    /// reopened / provider changed since the load began); otherwise merges the
    /// entitled rows into the open picker, preserving the filter + selection.
    pub(super) fn apply_catalog_result(
        &mut self,
        generation: u64,
        models: Vec<hrdr_agent::ChatGptModel>,
        source: hrdr_agent::CatalogSource,
        warning: Option<String>,
    ) {
        if generation != self.model_gen {
            return; // stale — a newer picker/provider superseded this load.
        }
        self.model_loading = false;
        // Surface the warning BEFORE the empty-rows check: a failed token refresh
        // returns no rows, and that is precisely the case the user needs told
        // about — the picker only opened because credentials looked present, so
        // silence reads as "you have no models" rather than "you are signed out".
        if let Some(w) = warning {
            self.system(w);
        }
        if models.is_empty() {
            return; // token/refresh failed — keep the cached rows.
        }
        self.model_source = Some(source);
        let provider = self.state().provider.clone();
        if let Some(sel) = &mut self.model_selector {
            let base = hrdr_agent::model_choices(&self.cfg, provider.as_deref());
            let usage = hrdr_agent::load_model_usage();
            let merged = hrdr_agent::merge_chatgpt_choices(base, &models, &usage);
            sel.replace_model_choices(merged);
        }
    }

    /// Close the `/model` picker and bump the generation so any in-flight catalog
    /// load is ignored when it lands.
    pub(super) fn close_model_selector(&mut self) {
        self.model_selector = None;
        self.model_loading = false;
        self.model_source = None;
        self.model_gen = self.model_gen.wrapping_add(1);
    }

    /// Cancel the `/theme` picker, restoring the theme in force when it opened.
    fn close_theme_selector(&mut self) {
        if self.theme_selector.take().is_some()
            && let Some(orig) = self.theme_original.take()
        {
            self.theme = orig;
        }
    }

    /// Live preview: paint the highlighted theme (the whole UI redraws with it
    /// next frame). No row under the filter → show the original again.
    pub(super) fn preview_selected_theme(&mut self) {
        let Some(sel) = &self.theme_selector else {
            return;
        };
        self.theme = match sel.current() {
            Some(c) => crate::theme::Theme::load(Some(&c.spec)),
            None => self.theme_original.clone().unwrap_or_default(),
        };
    }

    /// Apply the highlighted theme from the `/theme` picker, persist it as the
    /// config default, and close the picker.
    fn apply_selected_theme(&mut self) {
        let Some(sel) = self.theme_selector.take() else {
            return;
        };
        let original = self.theme_original.take();
        let Some(c) = sel.current().cloned() else {
            if let Some(orig) = original {
                self.theme = orig;
            }
            return;
        };
        self.theme = crate::theme::Theme::load(Some(&c.spec));
        self.persist_setting("theme", hrdr_agent::ConfigValue::Str(&c.spec));
        self.system(format!("theme → {} ({})", c.name, c.source));
    }

    /// Route one key to the open `/effort` picker: Esc/Ctrl+C closes it,
    /// Up/Down move the highlight, Enter applies the highlighted level, and
    /// any other character edits the fuzzy filter. Caller checks
    /// `self.effort_selector.is_some()` first.
    pub(super) fn effort_selector_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.effort_selector = None,
            KeyCode::Char('c') if ctrl => self.effort_selector = None,
            KeyCode::Up => {
                if let Some(s) = &mut self.effort_selector {
                    s.up();
                }
            }
            KeyCode::Down => {
                if let Some(s) = &mut self.effort_selector {
                    s.down();
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = &mut self.effort_selector {
                    s.backspace();
                }
            }
            KeyCode::Enter => self.apply_selected_effort(),
            KeyCode::Char(ch) if !ctrl => {
                if let Some(s) = &mut self.effort_selector {
                    s.push_char(ch);
                }
            }
            _ => {}
        }
    }

    /// Apply the highlighted effort from the `/effort` picker and close it:
    /// set (or, for "Default", clear) the status-bar label, persist the pick
    /// as the config default, and push it to the model client so it's sent as
    /// `reasoning_effort` (or an Anthropic thinking budget) on the next call.
    fn apply_selected_effort(&mut self) {
        let Some(c) = self
            .effort_selector
            .as_ref()
            .and_then(|s| s.current())
            .cloned()
        else {
            self.effort_selector = None;
            return;
        };
        self.effort_selector = None;
        match &c.value {
            Some(v) => self.persist_setting("effort", hrdr_agent::ConfigValue::Str(v)),
            None => self.unpersist_setting("effort"),
        }
        // The agent you are looking at, like every other command — and the agent
        // publishes the new effort itself, so the chrome follows without a copy.
        let agent = self.active_agent();
        let value = c.value.clone();
        tokio::spawn(async move {
            agent.lock().await.set_effort(value);
        });
        self.system(match &c.value {
            Some(v) => format!("effort → {} ({v})", c.label),
            None => "effort → default (the model/provider default)".to_string(),
        });
    }
}

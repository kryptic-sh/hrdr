use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Session;
use hrdr_agent::Agent;
use tokio::sync::Mutex;

use super::dispatch::open_system_handler;
use super::types::{ExpandMode, LineFuture, LineKind};

/// The capabilities a frontend exposes so the shared commands can drive it.
pub trait CommandHost {
    /// Emit a system line immediately (on the UI thread).
    fn info(&mut self, line: String);
    /// Spawn `fut`; when it resolves, show its non-empty string as a system line.
    fn spawn_line(&self, fut: LineFuture) {
        let poster = self.line_poster();
        tokio::spawn(async move {
            let line = fut.await;
            if !line.is_empty() {
                poster(LineKind::System, line);
            }
        });
    }
    /// The agent a command **acts on**: the one the user is looking at.
    ///
    /// A command that inspects or changes *a conversation* — `/compact`, `/model`,
    /// `/tools`, `/prompt`, `/status`, `/temp`, `/doctor` — acts on the agent on
    /// screen, exactly as the input box does. A frontend that shows only one agent
    /// simply returns it.
    fn agent(&self) -> Arc<Mutex<Agent>>;

    /// Working directory the tools operate in.
    fn cwd(&self) -> PathBuf;
    /// Current endpoint base URL (recorded into saved sessions).
    fn base_url(&self) -> String;

    // The chrome below describes **the agent `agent()` returns** — the one on
    // screen. `set_model_ref` must therefore write to *that* agent's state, not to
    // a display copy of the session's: `/model` in a sub-agent's view switches that
    // sub-agent, and the status bar shows it because it is reading the very same
    // state.

    /// What the agent on screen is running on: provider AND model, as one value.
    ///
    /// One accessor, because it is one thing. The old `model()`/`provider()` pair
    /// let a caller read half of it, act on that half, and leave the other half
    /// describing a provider the model has never been served by.
    fn model_ref(&self) -> hrdr_agent::ModelRef;

    /// Update the displayed identity (the agent itself is switched in the same step
    /// by [`apply_reference`](crate::commands::model), under its lock).
    fn set_model_ref(&mut self, reference: hrdr_agent::ModelRef);

    /// The model id alone — for the places that only render it.
    fn model(&self) -> String {
        self.model_ref().model().to_string()
    }
    /// The provider name alone — for the places that only render it.
    fn provider(&self) -> String {
        self.model_ref().provider().to_string()
    }

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
    /// The Nth (1-based) user/assistant message's text (for `/copy msg N[-M]`).
    fn nth_message_text(&self, n: usize) -> Option<String>;
    /// The most recent fenced code block (for `/copy code`). Default: from the
    /// last reply only; frontends may search further back.
    fn last_code_block(&self) -> Option<String> {
        self.last_reply()
            .as_deref()
            .and_then(crate::last_fenced_block)
    }

    /// A `Send`able closure that delivers an async result line onto the UI
    /// thread through the frontend's channel — the one primitive behind the
    /// [`spawn_line`](Self::spawn_line)/[`spawn_diff`](Self::spawn_diff)
    /// defaults (the host itself isn't `Send`, so they capture this).
    fn line_poster(&self) -> Box<dyn Fn(LineKind, String) + Send>;

    /// Like [`spawn_line`](Self::spawn_line), but the resolved string may be a
    /// unified diff: a real diff routes to the frontend's diff rendering,
    /// status/error lines stay plain (one classification rule for both).
    fn spawn_diff(&self, fut: LineFuture) {
        let poster = self.line_poster();
        tokio::spawn(async move {
            let line = fut.await;
            if line.is_empty() {
                return;
            }
            let kind = if line.starts_with("diff ") {
                LineKind::Diff
            } else {
                LineKind::System
            };
            poster(kind, line);
        });
    }

    /// Whether a turn is currently running — the busy-guard for commands that
    /// mutate turn-coupled state (`/retry`, `/undo`, `/compact`, `/cwd`, …).
    fn is_busy(&self) -> bool;
    /// Launch a model turn with `prompt`. `show_as_user` displays it as a user
    /// message (`/retry`); `false` keeps it out of the transcript (`/init`).
    fn send_prompt(&mut self, prompt: String, show_as_user: bool);
    /// Replace the input buffer (`/undo` puts the rewound message back).
    fn set_input(&mut self, text: String);
    /// Prepend text to the input buffer (`/add` attaches a file block).
    fn prepend_input(&mut self, text: String);
    /// Insert text at the input cursor (`/paste`).
    fn insert_input(&mut self, text: String);
    /// Read the OS clipboard as text (`/paste`). `None` = unavailable.
    fn read_clipboard(&self) -> Option<String> {
        None
    }
    /// Apply an `/expand` mode to the tool-output display, returning the
    /// status line to show (the expansion state lives in the frontend).
    fn set_tool_expansion(&mut self, mode: ExpandMode) -> String;
    /// Rewind the last user turn: pop it (and the reply) from the agent
    /// history *and* the display transcript, returning the user's text.
    /// `None` when there's nothing to rewind (or the agent is locked).
    fn rewind_last_turn(&mut self) -> Option<String>;

    /// Persist one setting to the user config file. Default writes directly;
    /// the TUI overrides to also suppress its config hot-reload.
    fn persist_setting(&mut self, key: &str, value: hrdr_agent::ConfigValue) {
        let _ = hrdr_agent::persist_setting(key, value);
    }
    /// The reasoning-effort label shown in status chrome.
    fn effort(&self) -> Option<String> {
        None
    }
    /// The session's display-name override (`/rename`), for `/status`.
    fn session_label(&self) -> Option<String> {
        None
    }
    /// The latest model call's `(prompt, completion)` token usage.
    fn context_usage(&self) -> Option<(u32, u32)> {
        None
    }
    /// The model's context window in tokens, if known.
    fn context_window(&self) -> Option<u32> {
        None
    }
    /// Session-cumulative `(input, output)` token counters.
    fn session_tokens(&self) -> (usize, usize) {
        (0, 0)
    }
    /// Session-cumulative estimated cost in USD (0 when nothing was priced).
    fn session_cost(&self) -> f64 {
        0.0
    }
    /// Update the effort label (persistence is dispatch's job).
    fn set_effort(&mut self, label: String) {
        let _ = label;
    }

    /// Called after `/cwd` switched the working directory (update dir/branch
    /// displays and invalidate any `@file` index).
    fn cwd_changed(&mut self, new: &Path) {
        let _ = new;
    }
    /// Called when files changed on disk outside a turn (`/revert`), so the
    /// `@file` completion index can be invalidated.
    fn files_changed(&mut self) {}

    /// Kick off a compaction pass on a background task (runs like a turn:
    /// input queues behind it, cancel aborts it). When it lands the frontend
    /// shows [`compaction_message`], resets stale context usage, autosaves on
    /// success, and resumes queued sends — same semantics in both.
    fn start_compaction(&mut self, instructions: Option<String>);
    /// `/compact`: announce and start (shared line + [`run_compaction`] core).
    fn compact(&mut self, instructions: Option<String>) {
        self.info("compacting conversation…".to_string());
        self.start_compaction(instructions);
    }

    /// Current per-message timestamp style (frontends with timestamp rendering
    /// override the pair).
    fn timestamp_style(&self) -> crate::TimestampStyle {
        crate::TimestampStyle::Relative
    }
    /// Apply a timestamp style (persistence is dispatch's job).
    fn set_timestamp_style(&mut self, style: crate::TimestampStyle) {
        let _ = style;
    }
    /// Turns a completed TODO stays visible before pruning.
    fn todo_ttl(&self) -> u64 {
        crate::DEFAULT_TODO_TTL
    }
    /// Apply a TODO lifetime (persistence is dispatch's job).
    fn set_todo_ttl(&mut self, turns: u64) {
        let _ = turns;
    }

    /// Apply a theme (a path to an hjkl theme TOML; `None` = the bundled
    /// default). Persistence is dispatch's job.
    fn set_theme(&mut self, path: Option<String>) {
        let _ = path;
    }
    /// Remove one setting from the user config file (`/theme` reset). Default
    /// writes directly; the TUI overrides to suppress its hot-reload.
    fn unpersist_setting(&mut self, key: &str) {
        let _ = hrdr_agent::remove_setting(key);
    }

    /// Open `path` in an editor (`/edit`). The default launches the OS default
    /// handler (`xdg-open` / `open` / `start`); the TUI overrides to suspend the
    /// terminal and run `$EDITOR` instead.
    fn open_editor(&mut self, path: PathBuf) {
        let line = match open_system_handler(&path) {
            Ok(()) => format!("opened {} in the system editor", path.display()),
            Err(e) => format!("couldn't open {}: {e}", path.display()),
        };
        self.info(line);
    }

    /// Current status-bar mode (frontends with a status bar override the pair).
    fn statusbar_mode(&self) -> crate::StatusBarMode {
        crate::StatusBarMode::Truncate
    }
    /// Apply a status-bar mode (persistence is dispatch's job).
    fn set_statusbar_mode(&mut self, mode: crate::StatusBarMode) {
        let _ = mode;
    }

    /// Re-read config and apply the live-changeable settings (frontends decide
    /// what that covers and emit their own status lines).
    fn reload_config(&mut self) {
        self.info("reload isn't supported here".to_string());
    }

    /// Resolve a provider preset by name (built-ins + `[providers.<name>]`
    /// from config). `None` = unknown / no provider support.
    fn resolve_provider(&self, name: &str) -> Option<hrdr_agent::ResolvedProvider> {
        let _ = name;
        None
    }
    /// Update the displayed endpoint after a provider switch (`/model` picker or `/login`).
    fn set_base_url(&mut self, url: String) {
        let _ = url;
    }
    /// Update the displayed context window after a provider switch (`/model` picker or `/login`).
    fn set_context_window(&mut self, tokens: Option<u32>) {
        let _ = tokens;
    }

    /// A `Send`able sink that delivers a freshly-probed context window onto the
    /// UI thread (the async analogue of [`set_context_window`](Self::set_context_window),
    /// like [`line_poster`](Self::line_poster)). Default drops the value; a
    /// frontend overrides it to route through its channel so a model/provider
    /// switch can honor the new model's advertised max context.
    fn context_window_poster(&self) -> Box<dyn Fn(u32) + Send> {
        Box::new(|_| {})
    }

    /// Begin the `/login` wizard. A frontend that supports it stashes
    /// [`LoginWizard::start`](crate::LoginWizard::start)'s result in a modal
    /// slot and routes subsequent submitted lines to
    /// [`LoginWizard::step`](crate::LoginWizard::step); the default reports it's
    /// unavailable.
    fn begin_login(&mut self) {
        self.info("/login isn't available in this frontend".to_string());
    }

    /// Open the interactive `/model` selector — a filterable list of every model
    /// across the configured providers. A frontend that supports it stashes the
    /// selector in a modal slot; the default lists the models as text instead.
    fn begin_model_selector(&mut self) {
        self.info("model selector isn't available in this frontend".to_string());
    }

    /// Open the `/model` selector restricted to `provider`'s models.
    ///
    /// The UI's answer to "you named a provider, but a provider is not a model"
    /// (see [`apply_provider_or_pick`](crate::apply_provider_or_pick)): after a
    /// `/login` to a provider that declares no default and that you have never used,
    /// the useful thing is a list of its models, not an error. The default falls
    /// back to the unfiltered picker.
    fn begin_model_selector_for(&mut self, provider: &str) {
        let _ = provider;
        self.begin_model_selector();
    }

    /// Open the interactive `/resume` session picker — a filterable list of
    /// saved sessions, newest first. A frontend that supports it stashes the
    /// selector in a modal slot; the default falls back to the text listing.
    fn begin_session_selector(&mut self) {
        self.info(crate::session_list_text());
    }

    /// Open the interactive `/skills` picker — the discovered `:skill`
    /// templates; picking one inserts `:name ` into the input. The default
    /// lists them as text.
    fn begin_skill_selector(&mut self) {
        let skills = crate::discover_skills(&self.cwd());
        if skills.is_empty() {
            self.info(
                "no skills yet — put Markdown prompt templates in .hrdr/skills/ (or \
                 .claude/commands/, ~/.config/hrdr/skills/), then invoke one with \
                 :name [arguments]"
                    .to_string(),
            );
            return;
        }
        let mut s = format!("{} skills (invoke with :name [arguments]):", skills.len());
        for sk in skills {
            s.push_str(&format!("\n  :{}", sk.name));
            if !sk.description.is_empty() {
                s.push_str(&format!(" — {}", sk.description));
            }
            s.push_str(&format!("  [{}]", sk.source));
        }
        self.info(s);
    }

    /// Open the interactive `/effort` picker — the reasoning levels the
    /// current model accepts (models.dev catalog), highest first, "Default"
    /// on top. A frontend that supports it stashes the selector in a modal
    /// slot; the default lists the levels as text.
    fn begin_effort_selector(&mut self) {
        let reference = self.model_ref();
        let choices = crate::effort_choices(Some(reference.provider().as_str()), reference.model());
        let mut s = format!(
            "effort: {} — levels for this model:",
            self.effort().unwrap_or_else(|| "default".into())
        );
        for c in choices {
            s.push_str(&format!("\n  {}", c.label));
            if !c.detail.is_empty() {
                s.push_str(&format!(" — {}", c.detail));
            }
        }
        self.info(s);
    }

    /// Open the interactive `/theme` picker — the baked-in themes plus any
    /// user theme files. A frontend that supports it stashes the selector in a
    /// modal slot; the default lists the choices as text.
    fn begin_theme_selector(&mut self) {
        let mut s = String::from("themes (apply with /theme <name or path>):");
        for c in crate::theme_choices() {
            s.push_str(&format!("\n  {}  [{}]", c.name, c.source));
        }
        self.info(s);
    }

    /// Whether this frontend supports `cmd` (used to filter `/help`).
    fn supports_command(&self, _cmd: &str) -> bool {
        true
    }

    /// Frontend-specific keybinding tips appended to `/help`.
    fn help_tips(&self) -> Option<String> {
        None
    }
}

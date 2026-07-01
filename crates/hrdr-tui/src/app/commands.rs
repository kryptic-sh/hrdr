//! Slash-command dispatch and the individual command handlers.

use super::*;
use crate::theme::Theme;
use hjkl_clipboard::{MimeType, Selection};
use hrdr_app::{last_fenced_block, parse_duration, parse_msg_range, resolve_alias, resolve_under};

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
        match cmd {
            "help" => self.system(help_text()),
            "clear" => {
                // Full reset — as if a fresh session just opened. `Agent::clear`
                // drops history and re-reads `AGENTS.md` (so an updated/removed
                // file is reflected); here we reset the view + interaction state.
                self.with_agent(|a| a.clear());
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
                self.system("conversation cleared");
            }
            "model" => {
                if arg.is_empty() {
                    self.system(format!("model: {}", self.model));
                } else {
                    if self.with_agent(|a| a.set_model(arg)).is_some() {
                        self.model = arg.to_string();
                        self.system(format!("model → {arg}"));
                    } else {
                        self.system("busy — try again after the current turn");
                    }
                }
            }
            "models" => self.list_models_cmd(),
            "provider" => self.switch_provider(arg),
            "theme" => {
                let path = (!arg.is_empty()).then_some(arg);
                self.theme = Theme::load(path);
                match path {
                    Some(p) => {
                        self.persist_setting("theme", hrdr_agent::ConfigValue::Str(p));
                        self.system(format!("theme → {p}"));
                    }
                    None => {
                        self.unpersist_setting("theme");
                        self.system("theme reset to default");
                    }
                }
            }
            "cwd" => self.change_cwd(arg),
            "tools" => self.show_tools(),
            "expand" => self.expand_cmd(arg),
            "revert" => self.revert_cmd(),
            "checkpoints" => self.checkpoints_cmd(),
            "add" => self.add_file(arg),
            "diff" => self.git_diff_cmd(),
            "thinking" | "reasoning" | "think" => self.thinking_cmd(arg),
            "temp" | "temperature" => self.set_temp_cmd(arg),
            "effort" => {
                if arg.is_empty() {
                    self.system(format!(
                        "effort: {}",
                        self.effort.clone().unwrap_or_else(|| "—".into())
                    ));
                } else {
                    self.effort = Some(arg.to_string());
                    self.persist_setting("effort", hrdr_agent::ConfigValue::Str(arg));
                    self.system(format!("effort → {arg}"));
                }
            }
            "info" => self.show_info(),
            "copy" => self.copy_cmd(arg),
            "export" => self.export_cmd(arg),
            "paste" => self.paste_cmd(),
            "retry" => self.retry_last(arg),
            "edit" => self.edit_file_cmd(arg),
            "undo" => self.undo_last(),
            "resume" | "load" => self.resume_session(arg),
            "rename" => self.rename_session(arg),
            "sessions" => self.list_sessions_cmd(arg),
            "compact" => self.compact_cmd(arg),
            "init" => self.init_agents_cmd(),
            "reload" => self.reload_cmd(),
            "goto" => self.goto_cmd(arg),
            "find" | "search" => self.find_cmd(arg),
            "next" => self.find_cycle(true),
            "prev" | "previous" => self.find_cycle(false),
            "timestamps" | "ts" => self.timestamps_cmd(arg),
            "statusbar" => self.statusbar_cmd(arg),
            "todo-ttl" | "todottl" | "todos" => self.todo_ttl_cmd(arg),
            _ => return false,
        }
        true
    }
    fn list_models_cmd(&mut self) {
        let Some(client) = self.with_agent_or_busy(|a| a.client()) else {
            return;
        };
        let tx = self.tx.clone();
        self.system("fetching models…");
        tokio::spawn(async move {
            let msg = match client.list_models().await {
                Ok(m) if !m.is_empty() => format!("models:\n  {}", m.join("\n  ")),
                Ok(_) => "endpoint reported no models".to_string(),
                Err(e) => format!("models error: {e}"),
            };
            let _ = tx.send(TurnMsg::System(msg));
        });
    }
    fn change_cwd(&mut self, arg: &str) {
        let Some(cur) = self.with_agent_or_busy(|a| a.cwd()) else {
            return;
        };
        if arg.is_empty() {
            self.system(format!("cwd: {}", cur.display()));
            return;
        }
        let new = resolve_under(&cur, arg);
        if !new.is_dir() {
            self.system(format!("not a directory: {}", new.display()));
            return;
        }
        let new = new.canonicalize().unwrap_or(new);
        self.apply_cwd(new.clone());
        self.system(format!("cwd → {}", new.display()));
    }
    /// `/expand [all|off]` — no arg toggles the most recent tool result's full
    /// view; `all` shows every tool result in full; `off` collapses everything.
    fn expand_cmd(&mut self, arg: &str) {
        match arg.trim().to_ascii_lowercase().as_str() {
            "all" | "on" => {
                self.expand_tools = true;
                self.system("tool output expanded (all)");
            }
            "off" | "none" | "collapse" => {
                self.expand_tools = false;
                for e in self.transcript.iter_mut() {
                    if let Entry::Tool { expanded, .. } = e {
                        *expanded = false;
                    }
                }
                self.system("tool output collapsed");
            }
            "" => {
                let last = self.transcript.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool { expanded, .. } => Some(expanded),
                    _ => None,
                });
                match last {
                    Some(expanded) => {
                        *expanded = !*expanded;
                        let now = *expanded;
                        self.system(if now {
                            "expanded last tool output"
                        } else {
                            "collapsed last tool output"
                        });
                    }
                    None => self.system("no tool output to expand"),
                }
            }
            _ => self.system("usage: /expand [all | off]"),
        }
    }
    fn show_tools(&mut self) {
        match self.with_agent(|a| a.tools()) {
            Some(tools) => {
                let mut s = String::from("tools:");
                for (n, d) in tools {
                    s.push_str(&format!("\n  {n} — {d}"));
                }
                self.system(s);
            }
            None => self.system("busy — try again after the current turn"),
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
        self.launch_turn(INIT_PROMPT.to_string());
    }
    fn add_file(&mut self, arg: &str) {
        if arg.is_empty() {
            self.system("usage: /add <file>");
            return;
        }
        let Some(cur) = self.with_agent_or_busy(|a| a.cwd()) else {
            return;
        };
        let path = resolve_under(&cur, arg);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let n = content.lines().count();
                let block = format!("`{arg}`:\n```\n{content}\n```\n\n");
                let existing = self.editor.content();
                self.editor.set_content(&format!("{block}{existing}"));
                self.system(format!("added {arg} ({n} lines) to the input"));
            }
            Err(e) => self.system(format!("can't read {arg}: {e}")),
        }
    }
    fn git_diff_cmd(&mut self) {
        let Some(cwd) = self.with_agent_or_busy(|a| a.cwd()) else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let out = tokio::process::Command::new("git")
                .arg("diff")
                .current_dir(&cwd)
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    if s.trim().is_empty() {
                        let _ = tx.send(TurnMsg::System("git diff: no changes".to_string()));
                    } else {
                        let _ = tx.send(TurnMsg::Diff(s));
                    }
                }
                Ok(o) => {
                    let _ = tx.send(TurnMsg::System(format!(
                        "git diff failed: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    )));
                }
                Err(e) => {
                    let _ = tx.send(TurnMsg::System(format!("git error: {e}")));
                }
            }
        });
    }
    fn set_temp_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            let t = self.with_agent(|a| a.temperature()).flatten();
            self.system(format!(
                "temperature: {}",
                t.map(|t| t.to_string()).unwrap_or_else(|| "default".into())
            ));
            return;
        }
        match arg.parse::<f32>() {
            Ok(t) => {
                self.with_agent(|a| a.set_temperature(Some(t)));
                self.persist_setting("temperature", hrdr_agent::ConfigValue::Float(t as f64));
                self.system(format!("temperature → {t}"));
            }
            Err(_) => self.system("usage: /temp <number>"),
        }
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
    fn undo_last(&mut self) {
        if self.running {
            self.system("can't undo while a turn is running");
            return;
        }
        let text = self
            .agent
            .try_lock()
            .ok()
            .and_then(|mut a| a.rewind_last_user());
        match text {
            Some(t) => {
                if let Some(idx) = self
                    .transcript
                    .iter()
                    .rposition(|e| matches!(e, Entry::User(_)))
                {
                    self.truncate_transcript(idx);
                }
                self.editor.set_content(&t); // restore for editing
                self.scroll_offset = 0;
                self.autosave();
                self.system("undid last turn — edit and resend");
            }
            None => self.system("nothing to undo"),
        }
    }
    fn switch_provider(&mut self, name: &str) {
        if name.is_empty() {
            self.system("usage: /provider <name>");
            return;
        }
        let Some(p) = self.cfg.resolve_provider(name) else {
            self.system(format!("unknown provider '{name}'"));
            return;
        };
        let key = p
            .api_key
            .clone()
            .or_else(|| p.key_env.as_ref().and_then(|e| std::env::var(e).ok()));
        let switched = self
            .with_agent(|a| {
                a.set_endpoint(p.base_url.clone(), key);
                if let Some(m) = &p.model {
                    a.set_model(m.clone());
                }
            })
            .is_some();
        if !switched {
            self.system("busy — try again after the current turn");
            return;
        }
        if let Some(m) = &p.model {
            self.model = m.clone();
        }
        if let Some(w) = p.context_window {
            self.context_window = Some(w);
        }
        self.base_url = p.base_url.clone();
        self.system(format!("provider → {name} ({})", p.base_url));
        if !p.remote {
            self.system(
                "note: a running backend isn't restarted; relaunch hrdr for a local backend",
            );
        }
    }
    /// `/copy [code|all|msg N]` — copy the last reply (default), the last code
    /// block, the whole transcript, or a specific numbered message.
    fn copy_cmd(&mut self, arg: &str) {
        let lower = arg.trim().to_ascii_lowercase();
        match lower.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] | ["reply"] | ["last"] => match self.last_assistant_text() {
                Some(t) => self.copy_to_clipboard(&t, "last reply"),
                None => self.system("no assistant reply to copy"),
            },
            ["code"] => match self.last_code_block() {
                Some(t) => self.copy_to_clipboard(&t, "last code block"),
                None => self.system("no code block to copy"),
            },
            ["all"] | ["transcript"] => {
                let t = self.transcript_text();
                if t.is_empty() {
                    self.system("nothing to copy");
                } else {
                    self.copy_to_clipboard(&t, "transcript");
                }
            }
            ["msg", spec] | ["message", spec] | ["m", spec] => self.copy_message_spec(spec),
            _ => self.system("usage: /copy [code | all | msg N | msg N-M]"),
        }
    }
    /// Copy a single message (`N`) or an inclusive range (`N-M`) by number.
    fn copy_message_spec(&mut self, spec: &str) {
        let Some((a, b)) = parse_msg_range(spec) else {
            self.system("usage: /copy msg <N> or <N-M>");
            return;
        };
        let parts: Vec<String> = (a..=b).filter_map(|n| self.nth_message_text(n)).collect();
        if parts.is_empty() {
            self.system(format!("no messages in {a}..{b} (see the #N tags)"));
            return;
        }
        let label = if a == b {
            format!("message #{a}")
        } else {
            format!("messages #{a}-{b}")
        };
        self.copy_to_clipboard(&parts.join("\n\n"), &label);
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
    /// `/statusbar [none|truncate|wrap]` — set the status-bar mode (no arg
    /// cycles truncate → wrap → none).
    fn statusbar_cmd(&mut self, arg: &str) {
        let mode = match arg.trim().to_ascii_lowercase().as_str() {
            "" => match self.statusbar_mode {
                StatusBarMode::Truncate => StatusBarMode::Wrap,
                StatusBarMode::Wrap => StatusBarMode::None,
                StatusBarMode::None => StatusBarMode::Truncate,
            },
            "none" | "off" | "hidden" => StatusBarMode::None,
            "truncate" | "trunc" => StatusBarMode::Truncate,
            "wrap" => StatusBarMode::Wrap,
            _ => {
                self.system("usage: /statusbar [none | truncate | wrap]");
                return;
            }
        };
        self.statusbar_mode = mode;
        self.persist_setting(
            "statusbar",
            hrdr_agent::ConfigValue::Str(mode.as_config_str()),
        );
        self.system(match mode {
            StatusBarMode::None => "status bar: hidden",
            StatusBarMode::Truncate => "status bar: truncate",
            StatusBarMode::Wrap => "status bar: wrap",
        });
    }
    /// `/thinking [on|off|1|0]` — show or hide the model's `<think>` reasoning
    /// blocks (no arg toggles). Persists as `show_thinking` in config. `/reasoning`
    /// is an alias.
    fn thinking_cmd(&mut self, arg: &str) {
        let arg = arg.trim();
        let on = if arg.is_empty() {
            !self.show_reasoning
        } else if let Some(b) = hrdr_agent::parse_env_bool(arg) {
            b
        } else {
            self.system("usage: /thinking [on | off]");
            return;
        };
        self.show_reasoning = on;
        self.persist_setting("show_thinking", hrdr_agent::ConfigValue::Bool(on));
        self.system(if on {
            "thinking shown"
        } else {
            "thinking hidden"
        });
    }
    /// `/todo-ttl [turns]` — how many turns a completed TODO stays visible
    /// before it's pruned from the panel. No arg reports the current value.
    fn todo_ttl_cmd(&mut self, arg: &str) {
        let arg = arg.trim();
        if arg.is_empty() {
            self.system(format!(
                "todo-ttl: {} turn{}",
                self.todo_ttl,
                if self.todo_ttl == 1 { "" } else { "s" }
            ));
            return;
        }
        match arg.parse::<u64>() {
            Ok(n) => {
                self.todo_ttl = n;
                self.persist_setting("todo_ttl", hrdr_agent::ConfigValue::Int(n as i64));
                self.system(format!(
                    "todo-ttl → {n} turn{}",
                    if n == 1 { "" } else { "s" }
                ));
            }
            Err(_) => self.system("usage: /todo-ttl <turns> (a whole number, e.g. 5)"),
        }
    }
    /// `/timestamps [none|relative|exact]` — set the timestamp style (no arg
    /// toggles between off and relative).
    fn timestamps_cmd(&mut self, arg: &str) {
        let style = match arg.trim().to_ascii_lowercase().as_str() {
            "" => {
                if self.timestamp_style == TimestampStyle::None {
                    TimestampStyle::Relative
                } else {
                    TimestampStyle::None
                }
            }
            "none" | "off" | "hidden" => TimestampStyle::None,
            "relative" | "rel" | "on" => TimestampStyle::Relative,
            "exact" | "absolute" | "abs" => TimestampStyle::Exact,
            _ => {
                self.system("usage: /timestamps [none | relative | exact]");
                return;
            }
        };
        self.timestamp_style = style;
        self.persist_setting(
            "timestamps",
            hrdr_agent::ConfigValue::Str(style.as_config_str()),
        );
        self.system(match style {
            TimestampStyle::None => "timestamps: off",
            TimestampStyle::Relative => "timestamps: relative",
            TimestampStyle::Exact => "timestamps: exact (HH:MM)",
        });
    }
    /// Write `text` to the system clipboard, reporting success/failure.
    fn copy_to_clipboard(&mut self, text: &str, label: &str) {
        let res = self
            .clipboard
            .as_mut()
            .map(|cb| cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes()));
        match res {
            Some(Ok(())) => self.system(format!("copied {label} to clipboard")),
            Some(Err(_)) => self.system("clipboard write failed"),
            None => self.system("clipboard unavailable"),
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
    /// `/paste` — insert the system clipboard into the input. If the clipboard
    /// holds a path to an existing file, attach it as an `@mention` instead.
    fn paste_cmd(&mut self) {
        let data = self
            .clipboard
            .as_ref()
            .and_then(|cb| cb.get(Selection::Clipboard, MimeType::Text).ok());
        let Some(bytes) = data else {
            self.system("clipboard unavailable or empty");
            return;
        };
        let text = String::from_utf8_lossy(&bytes).to_string();
        if text.is_empty() {
            self.system("clipboard is empty");
            return;
        }
        // A single-line path to an existing file → attach as `@path`.
        let trimmed = text.trim();
        if !trimmed.is_empty()
            && !trimmed.contains('\n')
            && let Some(cwd) = self.with_agent(|a| a.cwd())
        {
            let full = resolve_under(&cwd, trimmed);
            if full.is_file() {
                self.editor.paste(&format!("@{trimmed} "));
                self.system(format!("attached @{trimmed} from clipboard"));
                return;
            }
        }
        self.editor.paste(&text);
    }
    /// `/export [--json] [file]` — write the transcript to a file as text
    /// (default) or JSON. With no file, a timestamped `hrdr-transcript-<date>`
    /// in the cwd is used (`.md` or `.json`).
    fn export_cmd(&mut self, arg: &str) {
        let Some(cwd) = self.with_agent_or_busy(|a| a.cwd()) else {
            return;
        };
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
            Some(f) => resolve_under(&cwd, f),
            None => {
                let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                let ext = if json { "json" } else { "md" };
                cwd.join(format!("hrdr-transcript-{stamp}.{ext}"))
            }
        };
        let content = if json {
            self.transcript_json()
        } else {
            self.transcript_text()
        };
        match std::fs::write(&path, &content) {
            Ok(()) => self.system(format!(
                "exported transcript to {} ({} lines)",
                path.display(),
                content.lines().count()
            )),
            Err(e) => self.system(format!("export failed: {e}")),
        }
    }
    /// The conversation as a JSON array of `{n, role, time, content}` objects
    /// (user/assistant messages only).
    fn transcript_json(&self) -> String {
        hrdr_app::transcript_to_json(&self.transcript, &self.entry_times)
    }
    /// `/revert` — undo the most recent turn's file edits (restore pre-images).
    fn revert_cmd(&mut self) {
        if self.running {
            self.system("can't revert while a turn is running");
            return;
        }
        let Some(cp) = self.with_agent(|a| a.checkpoints()).flatten() else {
            self.system("checkpoints are off (auto-disabled in git repos — use git, or set checkpoints = on)");
            return;
        };
        let result = match cp.lock() {
            Ok(mut c) => c.revert_last(),
            Err(_) => {
                self.system("checkpoint store busy");
                return;
            }
        };
        match result {
            Ok(files) if files.is_empty() => self.system("nothing to revert"),
            Ok(files) => {
                let names = files
                    .iter()
                    .map(|p| {
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.display().to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                self.file_index_cwd = None; // files changed; rebuild @-completion index
                self.system(format!("reverted {} file(s): {names}", files.len()));
            }
            Err(e) => self.system(format!("revert failed: {e}")),
        }
    }
    /// `/checkpoints` — list the revertible per-turn file checkpoints.
    fn checkpoints_cmd(&mut self) {
        let Some(cp) = self.with_agent(|a| a.checkpoints()).flatten() else {
            self.system("checkpoints are off (auto-disabled in git repos — use git, or set checkpoints = on)");
            return;
        };
        let infos = match cp.lock() {
            Ok(c) => c.list(),
            Err(_) => {
                self.system("checkpoint store busy");
                return;
            }
        };
        if infos.is_empty() {
            self.system("no file checkpoints yet");
            return;
        }
        let mut s = String::from("file checkpoints (newest first; /revert undoes the latest):");
        for info in infos.iter().take(20) {
            let names = info
                .files
                .iter()
                .map(|f| {
                    std::path::Path::new(f)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| f.clone())
                })
                .collect::<Vec<_>>()
                .join(", ");
            s.push_str(&format!(
                "\n  turn {} · {} file(s): {names}",
                info.turn,
                info.files.len()
            ));
        }
        self.system(s);
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
    fn retry_last(&mut self, arg: &str) {
        if self.running {
            self.system("can't retry while a turn is running");
            return;
        }
        // Optional model switch for this retry (and subsequent turns).
        if !arg.is_empty() {
            if self.with_agent(|a| a.set_model(arg)).is_some() {
                self.model = arg.to_string();
                self.system(format!("model → {arg}"));
            } else {
                self.system("busy — try again after the current turn");
                return;
            }
        }
        let text = self
            .agent
            .try_lock()
            .ok()
            .and_then(|mut a| a.rewind_last_user());
        match text {
            Some(t) => {
                // Drop the old turn's transcript entries back to the last user message.
                if let Some(idx) = self
                    .transcript
                    .iter()
                    .rposition(|e| matches!(e, Entry::User(_)))
                {
                    self.truncate_transcript(idx);
                }
                self.scroll_offset = 0;
                self.spawn_turn(t);
            }
            None => self.system("nothing to retry"),
        }
    }
    /// `/compact [instructions]` — summarize the conversation to reclaim context.
    fn compact_cmd(&mut self, arg: &str) {
        if self.running {
            self.system("can't compact while a turn is running");
            return;
        }
        let count = self
            .agent
            .try_lock()
            .ok()
            .map(|a| a.message_count())
            .unwrap_or(0);
        if count <= 2 {
            self.system("nothing to compact yet");
            return;
        }
        let instructions = (!arg.trim().is_empty()).then(|| arg.trim().to_string());
        self.system("compacting conversation…");
        self.spawn_compaction(instructions);
    }
}

/// Render the grouped, aligned `/help` text: the shared command body plus the
/// TUI's own keybinding tips.
fn help_text() -> String {
    let mut s = hrdr_app::help_body();
    s.push_str(
        "\n\nTips: @path attaches a file · Up/Down recalls history · Ctrl+L redraws · \
         Ctrl+C twice quits",
    );
    s
}
/// Instruction sent to the model by `/init` to author an `AGENTS.md`.
const INIT_PROMPT: &str = "\
Analyze this codebase and create an AGENTS.md file at the repository root to guide \
AI coding agents working here (the open standard at https://agents.md).

Do this:
1. Explore the project with your tools — read the README(s), the build/manifest \
   files (Cargo.toml, package.json, pyproject.toml, go.mod, Makefile, etc.), CI \
   config, and skim the source layout with glob/grep/read_file to understand how \
   it's organized.
2. If an AGENTS.md (or CLAUDE.md / .cursorrules / similar) already exists, read it \
   and improve it instead of discarding useful content.
3. Write AGENTS.md (use the write_file tool) with concise, repo-specific sections:
   - Project overview: what it is and does.
   - Setup / build / run: the actual commands for THIS repo.
   - Testing: how to run the test suite and a single test.
   - Code style & conventions: formatting, linting, naming — inferred from config \
     and existing code.
   - Architecture / layout: key directories and how they fit together.
   - Gotchas or rules an agent must follow.

Prefer real commands, paths, and specifics over generic advice. Keep it tight. \
When finished, give a one-line summary of what you wrote.";

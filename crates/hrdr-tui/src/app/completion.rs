//! Slash-command and `@` mention completion (sub-agent names + file paths),
//! sharing one popup.

use hrdr_app::{
    active_file_token, arg_completions, command_arg_offset, command_match_key, prompt_completions,
    rank_agent_matches, rank_file_matches, resolve_alias, slash_completions,
};

impl super::App {
    /// The active completion popup contents: slash commands when the line
    /// starts with `/`, else sub-agent names + `@file` paths when an `@…`
    /// token is being typed. Both feed the same popup; only how the accepted
    /// item is inserted differs (see [`CompletionKind`]).
    ///
    /// Memoized on the editor content + [`super::App::completion_generation`]:
    /// the `@file` branch re-scans and re-ranks the whole index (up to 20,000
    /// paths) per call, which used to happen on every frame and every
    /// keystroke. The cached value is cloned (it is small) and recomputed only
    /// when the content or a completion input (`commands`, `skills`, sub-agent
    /// names, `file_index`) changed.
    pub(crate) fn active_completions(&mut self) -> Option<Completions> {
        if self.suppress_completions {
            return None;
        }
        let content = self.editor.content();
        if let Some((cached_content, cached_gen, cached)) = &self.completion_cache
            && *cached_content == content
            && *cached_gen == self.completion_generation
        {
            return cached.clone();
        }
        let result = self.compute_completions(&content);
        self.completion_cache = Some((content, self.completion_generation, result.clone()));
        result
    }

    /// Compute the completion popup from scratch — the expensive half of
    /// [`Self::active_completions`], which memoizes this.
    fn compute_completions(&mut self, content: &str) -> Option<Completions> {
        let slash = slash_completions(content);
        if !slash.is_empty() {
            return Some(Completions {
                kind: CompletionKind::Slash,
                anchor_col: 0,
                items: slash
                    .into_iter()
                    .map(|(n, d)| (n.to_string(), d.to_string()))
                    .collect(),
            });
        }
        let invocations = prompt_completions(content, &self.commands, &self.skills.skills);
        if !invocations.is_empty() {
            return Some(Completions {
                kind: CompletionKind::Command,
                anchor_col: 0,
                items: invocations,
            });
        }
        // Argument completion: "/cmd partial" / ":command partial" — enum
        // values, theme names, session ids, or a command's declared `args:`.
        if let Some((start, items)) = arg_completions(content, &self.commands) {
            return Some(Completions {
                kind: CompletionKind::Arg { token_start: start },
                anchor_col: content[..start].chars().count(),
                items,
            });
        }
        // File-path arguments for the commands that take one.
        if let Some((start, query)) = file_arg_token(content) {
            let items = self.file_completion_items(&query);
            if !items.is_empty() {
                return Some(Completions {
                    kind: CompletionKind::Arg { token_start: start },
                    anchor_col: content[..start].chars().count(),
                    items,
                });
            }
        }
        if let Some((start, query)) = active_file_token(content) {
            // Sub-agents first (an accepted `@name` routes the message to that
            // agent), then file paths.
            let mut items: Vec<(String, String)> =
                rank_agent_matches(&hrdr_app::agent_names(&self.agent), &query)
                    .into_iter()
                    .map(|n| (n, "sub-agent".to_string()))
                    .collect();
            items.extend(self.file_completion_items(&query));
            if !items.is_empty() {
                return Some(Completions {
                    kind: CompletionKind::Mention { token_start: start },
                    // Anchor the popup at the `@` (char column within the
                    // token's own line).
                    anchor_col: content[..start]
                        .rsplit('\n')
                        .next()
                        .map_or(0, |line| line.chars().count()),
                    items,
                });
            }
        }
        None
    }
    /// Apply the selected completion. `trailing_space` adds a space after the
    /// inserted text — Tab and Enter both pass `true` (accept into the input and
    /// keep editing); the `false` path is kept for callers that want the bare
    /// token with no trailing space.
    pub(super) fn apply_completion(
        &mut self,
        comp: &Completions,
        idx: usize,
        trailing_space: bool,
    ) {
        let chosen = &comp.items[idx].0;
        match comp.kind {
            CompletionKind::Slash | CompletionKind::Command => {
                if trailing_space {
                    self.editor.set_content(&format!("{chosen} "));
                } else {
                    self.editor.set_content(chosen);
                }
            }
            CompletionKind::Arg { token_start } => {
                let content = self.editor.content();
                let prefix = content.get(..token_start).unwrap_or("");
                if trailing_space {
                    self.editor.set_content(&format!("{prefix}{chosen} "));
                } else {
                    self.editor.set_content(&format!("{prefix}{chosen}"));
                }
            }
            CompletionKind::Mention { token_start } => {
                let content = self.editor.content();
                let prefix = content.get(..token_start).unwrap_or("");
                // Replace the partial `@…` token with `@<path> ` — a space so the
                // next mention/word is separate. A DIRECTORY candidate ends in `/`
                // and gets no space: `@src/` is a complete mention on its own (it
                // attaches the listing), but the common next move is to keep typing
                // to descend into it, and a space would end the token instead.
                let sep = if chosen.ends_with('/') { "" } else { " " };
                self.editor.set_content(&format!("{prefix}@{chosen}{sep}"));
            }
        }
    }
    /// Whether the highlighted completion is one the user has already typed in
    /// full — so there is nothing left to accept and Enter should submit rather
    /// than re-insert it. Compares the chosen label against the token already in
    /// the input, per completion kind.
    pub(super) fn completion_is_exact(&self, comp: &Completions, idx: usize) -> bool {
        let chosen = comp.items[idx].0.as_str();
        let content = self.editor.content();
        match comp.kind {
            // A `/…` or `:…` command is "already typed in full" when it dispatches
            // as-is — NOT merely when it equals the highlighted suggestion. `/clear`
            // is a real command even though the popup surfaces its canonical `/new`,
            // so comparing against the suggestion would wrongly treat it as partial.
            CompletionKind::Slash => hrdr_app::is_known_command(content.trim()),
            CompletionKind::Command => {
                let name = content.trim().trim_start_matches(':');
                let key = command_match_key(name);
                !name.is_empty()
                    && self
                        .commands
                        .iter()
                        .any(|c| command_match_key(&c.name) == key)
            }
            CompletionKind::Arg { token_start } => {
                content.get(token_start..).map(str::trim) == Some(chosen)
            }
            // Mentions never reach this (Enter always accepts them), but keep the
            // arm total: the `@…` token minus its sigil, already fully typed.
            CompletionKind::Mention { token_start } => {
                content
                    .get(token_start..)
                    .and_then(|t| t.strip_prefix('@'))
                    .map(str::trim)
                    == Some(chosen)
            }
        }
    }

    /// Build (and cache) the list of files under the cwd, then rank by `query`.
    fn file_completion_items(&mut self, query: &str) -> Vec<(String, String)> {
        self.ensure_file_index();
        rank_file_matches(&self.file_index, query)
            .into_iter()
            .map(|p| (p, String::new()))
            .collect()
    }
    /// Kick off an off-thread rebuild of `file_index` if it's stale for the
    /// current cwd. The popup shows once the [`TurnMsg::FileIndex`] result
    /// lands — walking a big tree must not stall the frame.
    fn ensure_file_index(&mut self) {
        if self.file_index_building {
            return;
        }
        let cwd = hrdr_app::agent_cwd(&self.agent);
        if self.file_index_cwd.as_deref() == Some(cwd.as_path()) {
            return;
        }
        self.file_index_building = true;
        // This walk runs after whatever invalidated the index, so it captures
        // the new state; a dirty marker set by a mid-build event is now moot
        // (the `FileIndex` handler re-invalidates if a *further* event lands).
        self.file_index_dirty = false;
        let tx = self.tx.clone();
        let sent_cwd = cwd.clone();
        hrdr_app::spawn_file_index(cwd, move |files| {
            // Sync (off-thread) callback — can't await; a one-shot index result
            // into an idle channel, so `try_send` suffices.
            let _ = tx.try_send(super::TurnMsg::FileIndex(sent_cwd, files));
        });
    }

    /// A filesystem change landed in the watched tree (create/rename/remove —
    /// external edits, a `git pull`, the agent's own writes). Invalidate the
    /// cached index; the next `@` keystroke rebuilds it. If a rebuild is
    /// already in flight, mark it so the `FileIndex` handler discards its
    /// result — the snapshot it walked predates this change.
    pub(super) fn on_file_index_dirty(&mut self) {
        self.file_index_dirty = true;
        self.file_index_cwd = None;
        self.bump_completion_generation();
    }

    /// (Re)start the recursive watcher on `dir` that feeds
    /// [`TurnMsg::FileIndexDirty`]. Any change that can alter the completion
    /// listing — creates, renames, removals, and modifies (some backends, e.g.
    /// FSEvents, surface a directory change that way) — invalidates the index;
    /// a pure read (`Access`) cannot. Invalidation is lazy (one walk on the
    /// next `@`), so a false positive is free. Dropping the previous watcher
    /// stops it; `None` when no watcher could be established.
    pub(super) fn arm_file_watcher(&mut self, dir: &std::path::Path) {
        use notify::{RecursiveMode, Watcher};
        let tx = self.tx.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let dirty = match res {
                Ok(ev) => !matches!(ev.kind, notify::EventKind::Access(_)),
                // A watcher error (e.g. a watch-limit hit) means events may be
                // missed — err towards invalidating.
                Err(_) => true,
            };
            if dirty {
                let _ = tx.try_send(super::TurnMsg::FileIndexDirty);
            }
        })
        .ok()
        .and_then(|mut w| w.watch(dir, RecursiveMode::Recursive).ok().map(|()| w));
        self.file_watcher = watcher;
    }
}

/// If `input` is a slash command whose argument is a file path (`/edit`,
/// `/add`) with the argument being typed, return the byte offset where the
/// argument starts and the partial path. Mirrors [`arg_completions`]'s
/// offset semantics; the candidate list comes from the frontend's file index.
fn file_arg_token(input: &str) -> Option<(usize, String)> {
    let rest = input.strip_prefix('/')?;
    let ws = rest.find(char::is_whitespace)?;
    if !matches!(resolve_alias(&rest[..ws]).as_str(), "edit" | "add") {
        return None;
    }
    let arg_start = command_arg_offset(rest)?;
    let partial = &input[arg_start..];
    // A path argument has no spaces; once one appears, stand down.
    if partial.chars().any(char::is_whitespace) {
        return None;
    }
    Some((arg_start, partial.to_string()))
}

/// The active completion popup's contents and kind.
#[derive(Clone)]
pub(crate) struct Completions {
    pub(crate) kind: CompletionKind,
    /// Char column (within the token's own line) where the completed token
    /// starts — the popup is anchored above this column of the input.
    pub(crate) anchor_col: usize,
    /// `(label, description)` rows; the label is the text inserted on accept.
    pub(crate) items: Vec<(String, String)>,
}
/// Which completion is active, and how to apply the selection.
#[derive(Clone)]
pub(crate) enum CompletionKind {
    /// Replace the whole input with the chosen command.
    Slash,
    /// Replace the whole input with the chosen `:command` invocation.
    Command,
    /// Replace the argument (everything from this byte offset on) with the
    /// chosen value — enum settings, theme names, session ids, command `args:`,
    /// and `/edit`/`/add` file paths.
    Arg { token_start: usize },
    /// Replace the `@…` token starting at this byte offset with `@<label> `
    /// (a sub-agent name or a file path — both insert the same way).
    Mention { token_start: usize },
}

//! Slash-command and @file completion.

use hrdr_app::{active_file_token, rank_file_matches, slash_completions};

impl super::App {
    /// The active completion popup contents: slash commands when the line starts
    /// with `/`, else `@file` paths when an `@…` token is being typed.
    pub(crate) fn active_completions(&mut self) -> Option<Completions> {
        let content = self.editor.content();
        let slash = slash_completions(&content);
        if !slash.is_empty() {
            return Some(Completions {
                kind: CompletionKind::Slash,
                items: slash
                    .into_iter()
                    .map(|(n, d)| (n.to_string(), d.to_string()))
                    .collect(),
            });
        }
        if let Some((start, query)) = active_file_token(&content) {
            let items = self.file_completion_items(&query);
            if !items.is_empty() {
                return Some(Completions {
                    kind: CompletionKind::File { token_start: start },
                    items,
                });
            }
        }
        None
    }
    /// Apply the selected completion. `trailing_space` adds a space after the
    /// inserted text (Tab keeps editing; a slash Enter omits it so the bare
    /// command submits).
    pub(super) fn apply_completion(
        &mut self,
        comp: &Completions,
        idx: usize,
        trailing_space: bool,
    ) {
        let chosen = &comp.items[idx].0;
        match comp.kind {
            CompletionKind::Slash => {
                if trailing_space {
                    self.editor.set_content(&format!("{chosen} "));
                } else {
                    self.editor.set_content(chosen);
                }
            }
            CompletionKind::File { token_start } => {
                let content = self.editor.content();
                // Replace the partial `@…` token with `@<path> ` (always a space
                // so the next mention/word is separate).
                let prefix = content.get(..token_start).unwrap_or("");
                self.editor.set_content(&format!("{prefix}@{chosen} "));
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
        let tx = self.tx.clone();
        let sent_cwd = cwd.clone();
        hrdr_app::spawn_file_index(cwd, move |files| {
            let _ = tx.send(super::TurnMsg::FileIndex(sent_cwd, files));
        });
    }
}

/// The active completion popup's contents and kind.
pub(crate) struct Completions {
    pub(crate) kind: CompletionKind,
    /// `(label, description)` rows; the label is the text inserted on accept.
    pub(crate) items: Vec<(String, String)>,
}
/// Which completion is active, and how to apply the selection.
pub(crate) enum CompletionKind {
    /// Replace the whole input with the chosen command.
    Slash,
    /// Replace the `@…` token starting at this byte offset with `@<path> `.
    File { token_start: usize },
}

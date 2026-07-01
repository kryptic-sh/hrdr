//! Slash-command and @file completion.

use super::commands::SLASH_COMMANDS;
use super::util::walk_files;

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
        let q = query.to_ascii_lowercase();
        let mut scored: Vec<(u8, usize, &String)> = self
            .file_index
            .iter()
            .filter_map(|p| {
                if q.is_empty() {
                    return Some((1u8, p.len(), p));
                }
                let lp = p.to_ascii_lowercase();
                let base = lp.rsplit('/').next().unwrap_or(&lp);
                if base.starts_with(&q) {
                    Some((0, p.len(), p))
                } else if lp.contains(&q) {
                    Some((1, p.len(), p))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
        scored
            .into_iter()
            .take(8)
            .map(|(_, _, p)| (p.clone(), String::new()))
            .collect()
    }
    /// Rebuild `file_index` if it's stale for the current cwd.
    fn ensure_file_index(&mut self) {
        let Some(cwd) = self
            .agent
            .try_lock()
            .ok()
            .map(|a| a.cwd())
            .or_else(|| std::env::current_dir().ok())
        else {
            return;
        };
        if self.file_index_cwd.as_deref() == Some(cwd.as_path()) && !self.file_index.is_empty() {
            return;
        }
        self.file_index = walk_files(&cwd);
        self.file_index_cwd = Some(cwd);
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
impl Completions {
    /// Popup title shown on the border.
    pub(crate) fn title(&self) -> &'static str {
        match self.kind {
            CompletionKind::Slash => " commands · Tab ",
            CompletionKind::File { .. } => " files · Tab ",
        }
    }
}
/// If an `@…` file mention is being typed at the end of `input`, return the byte
/// offset of the `@` and the partial query after it. Requires the `@` to start a
/// token (preceded by start-of-input or whitespace) with no whitespace after it.
pub(super) fn active_file_token(input: &str) -> Option<(usize, String)> {
    let at = input.rfind('@')?;
    // Must start a token.
    if at > 0 {
        let prev = input[..at].chars().next_back()?;
        if !prev.is_whitespace() {
            return None;
        }
    }
    let query = &input[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at, query.to_string()))
}
/// Commands matching the in-progress `/…` input (empty once a space is typed).
///
/// Matches the query (the text after `/`) against both the command name and its
/// description (case-insensitive substring), so e.g. `/list` surfaces `/help`
/// ("list commands"). Ranked: name-prefix, then name-substring, then
/// description-substring.
pub(crate) fn slash_completions(input: &str) -> Vec<(&'static str, &'static str)> {
    let Some(query) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    if query.is_empty() {
        return SLASH_COMMANDS.to_vec();
    }
    let q = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, (&'static str, &'static str))> = Vec::new();
    for &(name, desc) in SLASH_COMMANDS {
        let nl = name.trim_start_matches('/').to_ascii_lowercase();
        let rank = if nl.starts_with(&q) {
            0
        } else if nl.contains(&q) {
            1
        } else if desc.to_ascii_lowercase().contains(&q) {
            2
        } else {
            continue;
        };
        scored.push((rank, (name, desc)));
    }
    scored.sort_by_key(|(r, _)| *r); // stable: preserves list order within a rank
    scored.into_iter().map(|(_, c)| c).collect()
}

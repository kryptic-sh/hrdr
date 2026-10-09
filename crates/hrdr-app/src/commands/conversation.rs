use std::path::{Path, PathBuf};

use hrdr_agent::{Message, MessageRole};

/// Write the conversation to a file per a `/export [--json] [file]` argument,
/// returning the path written and its line count. JSON when `--json` is passed
/// or the named file's extension is `.json` (case-insensitive); otherwise
/// Markdown. With no file, a timestamped `hrdr-transcript-<date>.{md,json}` in
/// `cwd` is used. An existing file is refused, never overwritten.
pub fn export_conversation(
    msgs: &[Message],
    cwd: &Path,
    arg: &str,
    unix_style_paths: bool,
) -> Result<(PathBuf, usize), String> {
    let mut json = false;
    let mut file: Option<&str> = None;
    for tok in arg.split_whitespace() {
        if tok == "--json" {
            json = true;
        } else if file.is_none() {
            file = Some(tok);
        } else {
            // A second filename (or an unknown token) used to be dropped
            // silently; refuse instead.
            return Err("usage: /export [--json] <file>".to_string());
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
    // The extension names the format even without the flag (`out.json` is JSON);
    // the flag itself forces JSON for any other name.
    if file.is_some_and(|f| {
        Path::new(f)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    }) {
        json = true;
    }
    if path.exists() {
        return Err(format!(
            "refusing to overwrite existing file: {}",
            hrdr_tools::display_path(&path, unix_style_paths)
        ));
    }
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

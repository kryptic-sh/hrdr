//! Free helper functions with no `App` receiver.

/// Run `$VISUAL`/`$EDITOR` (falling back to `vi`) on `path`, inheriting stdio.
/// The command string may carry args (e.g. `code -w`), split on whitespace.
pub(crate) fn run_editor(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
}

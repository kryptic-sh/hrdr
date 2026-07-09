//! Free helper functions with no `App` receiver.

use std::time::Duration;

/// Human-readable duration: `843ms` (<1s), `2.3s` (<1m), `1m 23s` (≥1m).
pub(super) fn format_duration(d: Duration) -> String {
    let millis = d.as_millis();
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let mins = d.as_secs() / 60;
    let remain_secs = d.as_secs() % 60;
    format!("{mins}m {remain_secs}s")
}
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

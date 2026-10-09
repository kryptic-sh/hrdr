//! Path helpers shared across hrdr's on-disk state (sessions, per-project
//! memory): all of them partition by working directory using the same slug, so
//! they must agree on how it's computed. Plus the one display helper —
//! [`display_dir`] — that both the agent (command sources) and the frontends
//! (chrome, pickers) render paths with, so they never disagree about where a
//! `~` goes.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

/// The shared flattening core behind every slug: trim, keep only alphanumerics
/// (everything else becomes `-`), collapse runs of separators, and lowercase.
/// `cwd_slug` and the sub-agent transcript ids both need the same
/// "a label becomes a safe file-name component" step, and both must agree on it.
pub(crate) fn flatten_slug(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

/// Slug for a working directory — the per-cwd subdirectory name. The full path
/// is flattened (e.g. `/home/me/Projects/foo` → `home-me-projects-foo`). A hash
/// of the original path is appended to avoid collisions between distinct paths
/// that map to the same slug (e.g. `foo-bar` vs `foo_bar`).
pub fn cwd_slug(cwd: &str) -> String {
    let s = flatten_slug(cwd);
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    let suffix = format!("-{:016x}", hasher.finish());
    if s.is_empty() {
        format!("root{suffix}")
    } else {
        format!("{s}{suffix}")
    }
}

/// Display form of `dir`, with the home directory collapsed to `~`.
pub fn display_dir(dir: &Path) -> String {
    let s = dir.to_string_lossy();
    match crate::agents_dir::home_dir() {
        Some(home) => collapse_home(&s, &home.to_string_lossy()),
        None => s.into_owned(),
    }
}

/// Display form of `dir`, collapsing home before applying the chosen separators.
pub fn display_dir_with_style(dir: &Path, unix_style_paths: bool) -> String {
    hrdr_tools::display_path(Path::new(&display_dir(dir)), unix_style_paths)
}

/// Render a discovery source only when discovery supplied its raw path.
pub fn display_discovery_source(
    label: &str,
    path: Option<&Path>,
    unix_style_paths: bool,
) -> String {
    path.map_or_else(
        || label.to_string(),
        |path| display_dir_with_style(path, unix_style_paths),
    )
}

/// Collapse `home` at a path boundary in `path` to `~`. A prefix match alone
/// isn't enough: `home = /home/mx` would strip the `/home/mx` off
/// `/home/mxaddict/proj` too, collapsing it to the bogus `~addict/proj`. Only
/// collapse when the match lands on a path boundary — the prefix is the whole
/// string, or the next char is a separator. Pure, so it's testable without
/// touching the process-wide `HOME`.
fn collapse_home(path: &str, home: &str) -> String {
    if !home.is_empty()
        && let Some(rest) = path.strip_prefix(home)
        && (rest.is_empty() || rest.starts_with(std::path::is_separator))
    {
        return format!("~{rest}");
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Home-boundary cases exercise the pure core without process-wide env mutation.

    #[test]
    fn explicit_style_collapses_home_before_rendering_and_preserves_identity() {
        let home = crate::agents_dir::home_dir().expect("sandbox home");
        let child = home.join("nested").join("child");
        let sibling = home.with_file_name(format!(
            "{}-other",
            home.file_name().unwrap().to_string_lossy()
        ));
        let raw = child.to_string_lossy().into_owned();
        let slug = cwd_slug(&raw);
        for style in [false, true] {
            assert_eq!(display_dir_with_style(&home, style), "~");
            let expected = if cfg!(windows) && !style {
                r"~\nested\child"
            } else {
                "~/nested/child"
            };
            assert_eq!(display_dir_with_style(&child, style), expected);
            let sibling_expected = if cfg!(windows) && style {
                sibling.to_string_lossy().replace('\\', "/")
            } else {
                sibling.to_string_lossy().into_owned()
            };
            assert_eq!(display_dir_with_style(&sibling, style), sibling_expected);
            assert_eq!(child.to_string_lossy(), raw);
            assert_eq!(cwd_slug(&raw), slug);
        }
    }

    #[cfg(unix)]
    #[test]
    fn explicit_style_preserves_unix_literal_backslashes() {
        let home = crate::agents_dir::home_dir().expect("sandbox home");
        let sibling = format!("{}\\literal", home.display());
        for style in [false, true] {
            assert_eq!(display_dir_with_style(Path::new(&sibling), style), sibling);
            assert_eq!(
                display_dir_with_style(&home.join(r"literal\name"), style),
                r"~/literal\name"
            );
        }
    }

    #[test]
    fn empty_home_does_not_collapse() {
        assert_eq!(collapse_home("/proj", ""), "/proj");
        assert_eq!(collapse_home("", ""), "");
    }

    #[cfg(windows)]
    #[test]
    fn windows_home_collapses_without_changing_suffix_separators() {
        let home = r"C:\Users\mx";
        assert_eq!(collapse_home(home, home), "~");
        assert_eq!(collapse_home(r"C:\Users\mx\proj", home), r"~\proj");
        assert_eq!(collapse_home(r"C:\Users\mx/proj\src", home), r"~/proj\src");
        assert_eq!(
            collapse_home(r"C:\Users\mxaddict\proj", home),
            r"C:\Users\mxaddict\proj"
        );
    }

    #[cfg(windows)]
    #[test]
    fn display_dir_uses_sandboxed_userprofile_without_home() {
        let home = std::env::var_os("HOME");
        let profile = std::env::var_os("USERPROFILE");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "paths::tests::display_dir_userprofile_child",
                "--ignored",
                "--nocapture",
            ])
            .env(hrdr_test_support::WINDOWS_USERPROFILE_CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "child failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("USERPROFILE fallback assertions completed"));
        assert_eq!(std::env::var_os("HOME"), home);
        assert_eq!(std::env::var_os("USERPROFILE"), profile);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "run by display_dir_uses_sandboxed_userprofile_without_home in a private process"]
    fn display_dir_userprofile_child() {
        let root = hrdr_test_support::sandbox_root();
        assert_eq!(
            root,
            std::env::temp_dir().join(format!("hrdr-test-sandbox-{}", std::process::id()))
        );
        let home = root.join("home");
        assert!(std::env::var_os("HOME").is_none());
        assert!(std::env::var_os(hrdr_test_support::WINDOWS_USERPROFILE_CHILD_ENV).is_none());
        assert_eq!(
            std::env::var_os("USERPROFILE").as_deref(),
            Some(home.as_os_str())
        );
        for (var, dir) in [
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_RUNTIME_DIR", "runtime"),
        ] {
            let expected = root.join(dir);
            assert_eq!(std::env::var_os(var).as_deref(), Some(expected.as_os_str()));
            assert!(expected.is_dir());
            hrdr_test_support::assert_sandboxed(&expected);
        }
        assert!(home.is_dir());
        assert_eq!(display_dir(&home.join("child")), r"~\child");
        let sibling = root.join("home-other").join("child");
        assert_eq!(display_dir(&sibling), sibling.to_string_lossy());
        println!("USERPROFILE fallback assertions completed");
    }

    #[cfg(unix)]
    #[test]
    fn unix_literal_backslash_is_not_a_home_boundary() {
        assert_eq!(
            collapse_home(r"/home/mx\proj", "/home/mx"),
            r"/home/mx\proj"
        );
    }

    #[test]
    fn display_dir_collapses_home_at_a_path_boundary() {
        assert_eq!(collapse_home("/home/mx", "/home/mx"), "~");
        assert_eq!(collapse_home("/home/mx/proj", "/home/mx"), "~/proj");
    }

    /// Regression: a bare prefix match turned `/home/mxaddict/proj` (a sibling
    /// directory that merely starts with the same characters as HOME) into
    /// the bogus `~addict/proj` — `mx` is not a path component of
    /// `mxaddict`, so it must not collapse at all.
    #[test]
    fn display_dir_does_not_collapse_a_sibling_directory_sharing_a_prefix() {
        assert_eq!(
            collapse_home("/home/mxaddict/proj", "/home/mx"),
            "/home/mxaddict/proj"
        );
    }
}

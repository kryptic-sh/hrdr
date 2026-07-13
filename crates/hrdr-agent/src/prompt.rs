//! System-prompt assembly via minijinja.
//!
//! hrdr uses Jinja for its *own* prompt templating only — the model wire-format
//! chat template is applied server-side (e.g. by infr). Keep that boundary:
//! we emit structured messages, the server renders the model prompt.

use std::path::Path;

use anyhow::{Context, Result};
use hrdr_tools::ToolRegistry;
use minijinja::{Environment, context};

const SYSTEM_TEMPLATE: &str = include_str!("templates/system.j2");

/// Render the agent system prompt for the given tool set and working directory.
/// `instructions` is the gathered AGENTS.md content (see [`gather_agent_docs`]).
///
/// Only the tool *names* are inlined — the full name/description/schema defs
/// go out natively with every request, so repeating descriptions here would
/// pay their tokens twice.
pub fn render_system(
    tools: &ToolRegistry,
    cwd: &Path,
    instructions: Option<&str>,
) -> Result<String> {
    let mut env = Environment::new();
    env.add_template("system", SYSTEM_TEMPLATE)
        .context("loading system template")?;
    let tmpl = env.get_template("system")?;

    let tool_names = tools
        .defs()
        .into_iter()
        .map(|d| d.function.name)
        .collect::<Vec<_>>()
        .join(", ");

    tmpl.render(context! {
        cwd => cwd.display().to_string(),
        os => os_context(),
        tool_names => tool_names,
        // Gate the edit/git guidance: a purely read-only sub-agent has no
        // mutating tools, so those sections would be dead weight (and mildly
        // contradict its persona).
        can_write => tools.has_write_tool(),
        instructions => instructions,
    })
    .context("rendering system template")
}

/// One-line OS description for the system prompt: kernel/family, the distro
/// (from `/etc/os-release` on Linux), and the system package manager actually
/// installed — so "install X system-wide" reaches for pacman on Arch, apt on
/// Debian/Ubuntu, brew on macOS, winget on Windows, etc.
fn os_context() -> String {
    let mut out = String::from(std::env::consts::OS);
    if let Some(distro) = linux_distro() {
        out.push_str(&format!(" ({distro})"));
    }
    if let Some(pm) = detect_package_manager() {
        out.push_str(&format!(" — system package manager: {pm}"));
    }
    out
}

/// The distro's `PRETTY_NAME` from `/etc/os-release` (Linux only).
fn linux_distro() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// First system package manager found on PATH, in this OS's conventional
/// order of preference.
fn detect_package_manager() -> Option<&'static str> {
    let candidates: &[&str] = if cfg!(windows) {
        &["winget", "scoop", "choco"]
    } else if cfg!(target_os = "macos") {
        &["brew", "port"]
    } else {
        &[
            "pacman",
            "apt-get",
            "dnf",
            "yum",
            "zypper",
            "apk",
            "xbps-install",
            "emerge",
            "nix-env",
            "pkg",
        ]
    };
    candidates.iter().copied().find(|p| which::which(p).is_ok())
}

/// File name for the open-standard project instructions (https://agents.md).
const AGENTS_FILE: &str = "AGENTS.md";

/// Collect project instructions from `AGENTS.md` files, walking from `cwd` up to
/// the filesystem root, plus global instruction files from standard locations.
/// Less specific files (system, then user-global, then ancestors) come first so
/// nearer files override by appearing later. Returns `None` if nothing is found.
pub fn gather_agent_docs(cwd: &Path) -> Option<String> {
    // Walk up from cwd; collect cwd-first (most specific first).
    let mut docs: Vec<String> = Vec::new();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Ok(text) = std::fs::read_to_string(d.join(AGENTS_FILE)) {
            let text = text.trim();
            if !text.is_empty() {
                docs.push(text.to_string());
            }
        }
        dir = d.parent();
    }
    // Reverse to outer-first (root ancestor … cwd).
    docs.reverse();

    // A single global instruction file, if any — first match wins.
    // Priority: hrdr → agents → opencode → claude.
    let mut global_paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = crate::config_dir() {
        global_paths.push(dir.join(AGENTS_FILE));
    }
    for app in &["agents", "opencode"] {
        if let Ok(d) = hjkl_xdg::config_dir(app) {
            global_paths.push(d.join(AGENTS_FILE));
        }
    }
    if let Some(home) = crate::agents_dir::home_dir() {
        global_paths.push(home.join(".claude/CLAUDE.md"));
    }
    if let Some(path) = global_paths.iter().find(|p| p.is_file())
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let text = text.trim();
        if !text.is_empty() {
            docs.insert(0, text.to_string());
        }
    }

    if docs.is_empty() {
        None
    } else {
        Some(docs.join("\n\n---\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_inlines_names_only_and_rules() {
        let tools = ToolRegistry::with_defaults();
        let p = render_system(&tools, Path::new("/tmp/x"), None).unwrap();
        // Tool names present, one line, but not their long descriptions
        // (those ship natively as function defs — no double token spend).
        assert!(p.contains("read"));
        assert!(p.contains("todo"));
        assert!(!p.contains("Replace an exact substring"));
        // The pitfall rules the guardrails enforce are also stated up front.
        assert!(p.contains("git add -A"));
        assert!(p.contains("force-push"));
        assert!(p.contains("old_string"));
        assert!(p.contains("/tmp/x"));
        assert!(!p.contains("Project instructions"));
        // The OS line names the platform (and, where detectable, the distro +
        // package manager) so system-wide installs use the right tool.
        assert!(p.contains(&format!("- OS: {}", std::env::consts::OS)));
    }

    #[test]
    fn read_only_tool_set_omits_edit_and_git_guidance() {
        let mut tools = ToolRegistry::with_defaults();
        let ro = tools.read_only_names();
        tools.retain_only(&ro);
        let p = render_system(&tools, Path::new("/tmp/x"), None).unwrap();
        // No mutating tools → the editing/git sections are dropped entirely.
        assert!(!p.contains("old_string"), "{p}");
        assert!(!p.contains("git add -A"), "{p}");
        assert!(!p.contains("force-push"), "{p}");
        assert!(!p.contains("Read a file before editing it"), "{p}");
        // Nothing it can reach can destroy anything, so the deletion rules would
        // be advice about tools it does not have.
        assert!(!p.contains("Deleting:"), "{p}");
        // The read/search workflow and the confinement safety line remain.
        assert!(p.contains("grep/find/ls/tree/read"), "{p}");
        assert!(p.contains("confined to the working directory"), "{p}");
    }

    /// Staging is by name, always — and the prompt says *why*, because a rule
    /// without a reason is one the model talks itself out of when it is in a hurry
    /// and the working tree is dirty.
    ///
    /// `git add -A` in someone else's repo commits whatever else happens to be
    /// lying around: their half-finished change, a scratch file, a build artifact,
    /// a file with a key in it. The agent cannot see far enough to know, so it does
    /// not get to use the wildcard.
    #[test]
    fn the_prompt_forbids_wildcard_staging_and_says_why() {
        let tools = ToolRegistry::with_defaults();
        let p = render_system(&tools, Path::new("/tmp/x"), None).unwrap();
        for forbidden in ["git add -A", "git add --all", "git add .", "git commit -a"] {
            assert!(
                p.contains(forbidden),
                "the prompt must name `{forbidden}` as forbidden, or the model \
                 will find the one spelling that was left out"
            );
        }
        assert!(
            p.contains("git add <file>"),
            "it must say what to do instead"
        );
        assert!(
            p.contains("git status --short"),
            "and how to find the names when it doesn't know them"
        );
    }

    /// Deletion is by explicit name, never by expansion — and the prompt says why.
    ///
    /// `rm -rf "$DIR"/*` with `DIR` unset is `rm -rf /*`. A glob deletes whatever
    /// it matches *at the moment it runs*, which is not the list the model
    /// reasoned about. Command substitution (`rm -rf $(find …)`) lets one command
    /// both pick the victims and kill them, with nobody reading the list in
    /// between. Each of those has eaten someone's home directory, so each is named
    /// here rather than left to inference from a general principle.
    #[test]
    fn the_prompt_forbids_deleting_by_expansion_and_says_why() {
        let tools = ToolRegistry::with_defaults();
        let p = render_system(&tools, Path::new("/tmp/x"), None).unwrap();

        for forbidden in [
            r#"rm -rf "$DIR""#,
            r#"rm -rf "$DIR"/*"#,
            "rm -rf $(...)",
            "find … -delete",
            "| xargs rm",
        ] {
            assert!(
                p.contains(forbidden),
                "the prompt must name `{forbidden}` as forbidden, or the model \
                 will reach for the spelling that was left out"
            );
        }
        // The failure mode, stated — not just the ban.
        assert!(
            p.contains("becomes `rm -rf /*`"),
            "it must say what an unset variable expands to"
        );
        // What to do instead.
        assert!(p.contains("rm file-a.txt file-b.txt"), "name the files");
        assert!(
            p.contains("read the list, then delete the entries by name"),
            "find out the names first, in a separate command"
        );
        // Irreversible actions in general, not just rm.
        for risky in ["TRUNCATE", "terraform destroy", "kubectl delete", "sed -i"] {
            assert!(p.contains(risky), "`{risky}` is irreversible too");
        }
        // And the reason models actually reach for `rm`: to make an error go away.
        assert!(
            p.contains("Deleting is not a way around a problem"),
            "clearing state to silence a failure is the habit to break"
        );
    }

    #[test]
    fn system_prompt_appends_project_instructions() {
        let tools = ToolRegistry::with_defaults();
        let p = render_system(&tools, Path::new("/tmp/x"), Some("Use tabs.")).unwrap();
        assert!(p.contains("Project instructions"));
        assert!(p.ends_with("Use tabs."));
    }

    #[test]
    fn gather_agent_docs_loads_project_via_cwd_walk() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("project");
        std::fs::create_dir(&proj).unwrap();
        let mut f = std::fs::File::create(proj.join("AGENTS.md")).unwrap();
        writeln!(f, "Project-level").unwrap();

        // No env mutation: `gather_agent_docs` collects *all* docs (project +
        // any global), and we only assert the project one was picked up by the
        // cwd walk — true regardless of the machine's global files. Mutating
        // HOME/XDG here used to race concurrent tests (`set_var` is process-wide
        // and unsafe under any parallel getenv), a source of CI-only flakes.
        let docs = gather_agent_docs(&proj).unwrap();
        assert!(docs.contains("Project-level"));
    }
}

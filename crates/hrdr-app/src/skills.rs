//! Custom skills: reusable prompt templates invoked with a `:` prefix
//! (`:name args…`), shared by hrdr's frontends.
//!
//! A skill is a Markdown file — optional `name:` / `description:` frontmatter,
//! body = the prompt. On invocation the body is sent to the model with
//! `$ARGUMENTS` replaced by everything after the skill name (or, when the
//! placeholder is absent and arguments were given, with them appended on their
//! own line). Discovery mirrors the sub-agent files: project dirs first, then
//! user dirs, hrdr → Claude Code → opencode conventions, deduped by name
//! (first source wins).

use std::path::{Path, PathBuf};

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Invocation name (`:name`) — frontmatter `name:`, else the file stem.
    pub name: String,
    /// One-line summary for the completion popup / `/skills` listing.
    pub description: String,
    /// The prompt template (the file body).
    pub body: String,
    /// Where it came from, for the `/skills` listing (home-shortened dir).
    pub source: String,
}

/// The skill directories to scan, in precedence order (highest first).
fn skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project scopes (nearest / most specific) first.
    dirs.push(cwd.join(".hrdr").join("skills"));
    dirs.push(cwd.join(".claude").join("commands"));
    dirs.push(cwd.join(".opencode").join("command"));
    // User scopes.
    if let Some(d) = hrdr_agent::config_dir() {
        dirs.push(d.join("skills")); // ~/.config/hrdr/skills
    }
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        dirs.push(home.join(".claude").join("commands"));
    }
    if let Ok(d) = hjkl_xdg::config_dir("opencode") {
        dirs.push(d.join("command")); // ~/.config/opencode/command
    }
    dirs
}

/// Discover skill files across the hrdr/Claude/opencode locations, relative to
/// `cwd` for project scopes. One skill per unique name (case-insensitive);
/// the first source in precedence order wins.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for dir in skill_dirs(cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<Skill> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    return None;
                }
                let text = std::fs::read_to_string(&path).ok()?;
                let stem = path.file_stem()?.to_str()?;
                parse_skill_file(&text, stem, &crate::display_dir(&dir))
            })
            .collect();
        // Stable order within a directory (read_dir order is unspecified).
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in found {
            if !out.iter().any(|s| s.name.eq_ignore_ascii_case(&skill.name)) {
                out.push(skill);
            }
        }
    }
    out
}

/// Parse one skill file: optional flat `name:`/`description:` frontmatter
/// (a leading `---` … `---` fence), body = the prompt. `None` when the body
/// is empty.
pub fn parse_skill_file(text: &str, filename_stem: &str, source: &str) -> Option<Skill> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (name, description, body) = match fenced_frontmatter(text) {
        Some((fm, body)) => {
            let field = |key: &str| {
                fm.lines().find_map(|l| {
                    l.strip_prefix(key)
                        .and_then(|r| r.strip_prefix(':'))
                        .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
                        .filter(|v| !v.is_empty())
                })
            };
            (field("name"), field("description"), body)
        }
        None => (None, None, text),
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(Skill {
        name: name.unwrap_or_else(|| filename_stem.to_string()),
        description: description.unwrap_or_default(),
        body: body.to_string(),
        source: source.to_string(),
    })
}

/// Split a leading `---` … `---` fence off `text`: `(frontmatter, body)`.
/// `None` when there is no (terminated) fence.
fn fenced_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?.strip_prefix('\n')?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// If `input` invokes a skill (`:name args…`, matched case-insensitively),
/// return the prompt to send: the skill body with every `$ARGUMENTS` replaced
/// by the arguments — or, when the body has no placeholder and arguments were
/// given, with them appended on their own line. `None` when the input isn't a
/// `:` invocation or names no known skill (it then goes to the model as-is).
pub fn expand_skill(input: &str, skills: &[Skill]) -> Option<String> {
    let rest = input.trim_start().strip_prefix(':')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().filter(|n| !n.is_empty())?;
    let args = parts.next().unwrap_or("").trim();
    let skill = skills.iter().find(|s| s.name.eq_ignore_ascii_case(name))?;
    Some(if skill.body.contains("$ARGUMENTS") {
        skill.body.replace("$ARGUMENTS", args)
    } else if args.is_empty() {
        skill.body.clone()
    } else {
        format!("{}\n\n{args}", skill.body)
    })
}

/// Skills matching an in-progress `:…` input (empty once a space is typed) as
/// `(":name", description)` rows for the completion popup. Ranked like the
/// slash commands: name-prefix, then name-substring, then description.
pub fn skill_completions(input: &str, skills: &[Skill]) -> Vec<(String, String)> {
    let Some(query) = input.strip_prefix(':') else {
        return Vec::new();
    };
    if query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let q = query.to_ascii_lowercase();
    let mut scored: Vec<(u8, (String, String))> = Vec::new();
    for s in skills {
        let nl = s.name.to_ascii_lowercase();
        let rank = if q.is_empty() || nl.starts_with(&q) {
            0
        } else if nl.contains(&q) {
            1
        } else if s.description.to_ascii_lowercase().contains(&q) {
            2
        } else {
            continue;
        };
        scored.push((rank, (format!(":{}", s.name), s.description.clone())));
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.0.cmp(&b.1.0)));
    scored.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, desc: &str, body: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            body: body.to_string(),
            source: "test".to_string(),
        }
    }

    #[test]
    fn parse_reads_frontmatter_and_falls_back_to_the_stem() {
        let s = parse_skill_file(
            "---\nname: ship\ndescription: release checklist\n---\nDo the release.",
            "stem",
            "src",
        )
        .unwrap();
        assert_eq!(s.name, "ship");
        assert_eq!(s.description, "release checklist");
        assert_eq!(s.body, "Do the release.");

        // No frontmatter: the stem names it, the whole text is the body.
        let s = parse_skill_file("Just a prompt.", "quick", "src").unwrap();
        assert_eq!(s.name, "quick");
        assert_eq!(s.body, "Just a prompt.");

        // Empty body → not a skill.
        assert!(parse_skill_file("---\nname: x\n---\n  \n", "x", "src").is_none());
    }

    #[test]
    fn expand_substitutes_arguments_or_appends() {
        let skills = vec![
            skill("review", "", "Review the diff.\nFocus: $ARGUMENTS"),
            skill("ship", "", "Run the release checklist."),
        ];
        // $ARGUMENTS placeholder is substituted (matched case-insensitively).
        assert_eq!(
            expand_skill(":Review error handling", &skills).unwrap(),
            "Review the diff.\nFocus: error handling"
        );
        // No placeholder: args append on their own line…
        assert_eq!(
            expand_skill(":ship v2 only", &skills).unwrap(),
            "Run the release checklist.\n\nv2 only"
        );
        // …and no args leaves the body untouched.
        assert_eq!(
            expand_skill(":ship", &skills).unwrap(),
            "Run the release checklist."
        );
        // Unknown name / not an invocation → None (sent to the model as-is).
        assert!(expand_skill(":nope", &skills).is_none());
        assert!(expand_skill("hello :ship", &skills).is_none());
        assert!(expand_skill(": ship", &skills).is_none());
    }

    #[test]
    fn discovery_dedupes_by_name_project_first() {
        let dir = tempfile::tempdir().unwrap();
        let hrdr = dir.path().join(".hrdr/skills");
        let claude = dir.path().join(".claude/commands");
        std::fs::create_dir_all(&hrdr).unwrap();
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(hrdr.join("ship.md"), "hrdr wins").unwrap();
        std::fs::write(claude.join("ship.md"), "claude loses").unwrap();
        std::fs::write(claude.join("review.md"), "review the diff").unwrap();
        std::fs::write(claude.join("notes.txt"), "not a skill").unwrap();

        let skills = discover_skills(dir.path());
        let ship = skills.iter().find(|s| s.name == "ship").unwrap();
        assert_eq!(ship.body, "hrdr wins", "project .hrdr dir outranks .claude");
        assert!(skills.iter().any(|s| s.name == "review"));
        assert!(!skills.iter().any(|s| s.name == "notes"));
    }

    #[test]
    fn completions_rank_prefix_then_substring_then_description() {
        let skills = vec![
            skill("ship", "release checklist", "…"),
            skill("review", "inspect a shipped diff", "…"),
        ];
        let names = |i: &str| {
            skill_completions(i, &skills)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(":"), vec![":review", ":ship"]);
        assert_eq!(names(":sh").first().map(String::as_str), Some(":ship"));
        // Description match surfaces :review for "diff".
        assert_eq!(names(":diff"), vec![":review"]);
        // A space kills completion; non-: input yields nothing.
        assert!(names(":ship ").is_empty());
        assert!(names("/ship").is_empty());
    }
}

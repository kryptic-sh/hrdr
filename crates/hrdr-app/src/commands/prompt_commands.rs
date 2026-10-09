//! Frontend half of the `:name` namespace — the `:`-completion popup and the
//! `/commands` picker. (The `/name` slash commands are the rest of this module.)
//!
//! Two things answer `:name`: **commands** (one flat `<name>.md` prompt template,
//! `$ARGUMENTS` and all) and **skills** (a `SKILL.md` bundle directory). Discovery,
//! parsing and expansion for both live in `hrdr-agent` — the model can invoke
//! either through the `command` / `skill` tools, so the agent owns that half and
//! every invocation path expands through the same code. Re-exported here so the
//! frontends keep referring to `hrdr_app::Command` / `hrdr_app::discover_commands`.
//!
//! What this file adds is the **shared view** of that one namespace:
//! [`PromptEntry`], the owned row both UI surfaces render. The two data types stay
//! separate (they parse differently, validate differently and carry different
//! fields); they meet only where the UI needs one flat list, which is also where
//! the collision rule becomes visible — a command wins `:name`, and the skill it
//! shadows is marked rather than hidden.

pub use hrdr_agent::{
    Command, DiscoveredSkills, InvalidSkill, ProjectInstructions, Skill, builtin_commands,
    command_match_key, discover_commands, discover_skills, expand_command, expand_invocation,
};

/// What a `:name` row is, and — for a skill — whether it is usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptEntryKind {
    /// A command. Always wins its name.
    Command,
    /// A skill bundle, invocable as `:name`.
    Skill,
    /// A skill something else already owns the name of, carrying what shadows
    /// it: a **command** (`:name` runs the command, but the bundle is still
    /// loadable by the model through the `skill` tool) or a **higher-precedence
    /// skill root** (nothing can reach this copy at all). Shown rather than
    /// dropped either way — "why is my skill not running" has to be answerable
    /// from this screen.
    ShadowedSkill(String),
    /// A `SKILL.md` that failed validation, with the reason — shown because a
    /// skill that silently does not appear is the format's usual complaint.
    InvalidSkill(String),
}

/// One row of the `:name` namespace as the pickers see it: a command or a skill,
/// flattened to what both surfaces render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptEntry {
    pub name: String,
    pub description: String,
    /// Where it came from (a home-shortened directory, or `built-in`).
    pub source: String,
    /// Raw discovery path retained so an open picker can restyle its source.
    pub source_path: Option<std::path::PathBuf>,
    pub kind: PromptEntryKind,
}

impl PromptEntry {
    /// Refresh only the path-backed source; authored labels stay untouched.
    pub fn restyle_source(&mut self, unix_style_paths: bool) {
        self.source = hrdr_agent::display_discovery_source(
            &self.source,
            self.source_path.as_deref(),
            unix_style_paths,
        );
    }

    /// The row's right-hand column: what it is, why it is unusable if it is, and
    /// its description. One definition so the TUI picker and the headless text
    /// listing say the same thing.
    pub fn detail(&self) -> String {
        let describe = |prefix: &str| match (prefix.is_empty(), self.description.is_empty()) {
            (true, true) => self.source.clone(),
            (true, false) => self.description.clone(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix} · {}", self.description),
        };
        match &self.kind {
            PromptEntryKind::Command => describe(""),
            PromptEntryKind::Skill => describe("skill"),
            PromptEntryKind::ShadowedSkill(by) => describe(&format!("skill, shadowed by {by}")),
            PromptEntryKind::InvalidSkill(reason) => format!("invalid skill: {reason}"),
        }
    }
}

/// The `:name` namespace as one list: every command, then every skill — the
/// shadowed and the invalid ones included and marked, since "my skill is not
/// showing up" is otherwise unanswerable from the UI. Both kinds of shadowing
/// are marked: a command owning the name, and a higher-precedence skill root
/// holding a bundle of the same name.
pub fn prompt_entries(
    commands: &[Command],
    skills: &DiscoveredSkills,
    unix_style_paths: bool,
) -> Vec<PromptEntry> {
    let mut out: Vec<PromptEntry> = commands
        .iter()
        .map(|c| PromptEntry {
            name: c.name.clone(),
            description: c.description.clone(),
            source: c.source.clone(),
            source_path: c.source_path.clone(),
            kind: PromptEntryKind::Command,
        })
        .collect();
    let shadowed_by_command = |name: &str| {
        let key = command_match_key(name);
        commands.iter().any(|c| command_match_key(&c.name) == key)
    };
    out.extend(skills.skills.iter().map(|s| PromptEntry {
        name: s.name.clone(),
        description: s.description.clone(),
        source: s.source.clone(),
        source_path: s.source_path.clone(),
        kind: if shadowed_by_command(&s.name) {
            PromptEntryKind::ShadowedSkill("a command".to_string())
        } else {
            PromptEntryKind::Skill
        },
    }));
    // A bundle a higher-precedence root already claimed the name of. It never
    // runs and the model never sees it, so the command that may also own the
    // name changes nothing about it: name the skill root that won.
    out.extend(skills.shadowed.iter().map(|s| PromptEntry {
        name: s.name.clone(),
        description: s.description.clone(),
        source: s.source.clone(),
        source_path: s.source_path.clone(),
        kind: PromptEntryKind::ShadowedSkill("a higher-precedence skill".to_string()),
    }));
    out.extend(skills.invalid.iter().map(|i| PromptEntry {
        name: i.name.clone(),
        description: String::new(),
        source: i.path.clone(),
        source_path: i.source_path.clone(),
        kind: PromptEntryKind::InvalidSkill(i.reason.clone()),
    }));
    for entry in &mut out {
        entry.restyle_source(unix_style_paths);
    }
    out
}

/// The lowercase haystack [`filter_prompt_entries`] matches against: the
/// space-joined `"name detail source"`, precomputed once per picker open. The
/// detail carries the kind, so typing `skill` narrows to the skills.
pub fn prompt_entry_haystack(entry: &PromptEntry) -> String {
    format!("{} {} {}", entry.name, entry.detail(), entry.source).to_lowercase()
}

/// Case-insensitive fuzzy filter over precomputed haystacks (built by
/// [`prompt_entry_haystack`]): the query's characters must appear in order within
/// the haystack. Returns matching indices in input order; an empty query matches
/// everything.
pub fn filter_prompt_entries(haystacks: &[String], query: &str) -> Vec<usize> {
    hrdr_agent::fuzzy_filter(haystacks, query)
}

/// Commands and skills matching an in-progress `:…` input (empty once a space is
/// typed) as `(":name", description)` rows for the completion popup. Ranked like
/// the slash commands: name-prefix, then name-substring, then description.
///
/// Names are matched through [`command_match_key`], so a namespace typed with
/// any of its three separators (`:git/`, `:git:`, `:git.`) narrows to the same
/// rows — but the row inserted is always the canonical `/` spelling.
///
/// A skill whose name a command already owns contributes no row: `:name` would
/// run the command, and offering the same text twice tells the user nothing. The
/// `/commands` picker is where that collision is spelled out.
pub fn prompt_completions(
    input: &str,
    commands: &[Command],
    skills: &[Skill],
) -> Vec<(String, String)> {
    let Some(query) = input.strip_prefix(':') else {
        return Vec::new();
    };
    if query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let q = command_match_key(query);
    let mut scored: Vec<(u8, (String, String))> = Vec::new();
    let mut push = |name: &str, description: &str| {
        let nl = command_match_key(name);
        let rank = if q.is_empty() || nl.starts_with(&q) {
            0
        } else if nl.contains(&q) {
            1
        } else if description.to_ascii_lowercase().contains(&q) {
            2
        } else {
            return;
        };
        scored.push((rank, (format!(":{name}"), description.to_string())));
    };
    for c in commands {
        push(&c.name, &c.description);
    }
    for s in skills {
        let key = command_match_key(&s.name);
        if commands.iter().any(|c| command_match_key(&c.name) == key) {
            continue;
        }
        push(&s.name, &s.description);
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.0.cmp(&b.1.0)));
    scored.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str, desc: &str) -> Command {
        Command {
            name: name.to_string(),
            description: desc.to_string(),
            body: "…".to_string(),
            source: "test".to_string(),
            source_path: None,
            args: Vec::new(),
            model_invocable: true,
        }
    }

    fn skill(name: &str, desc: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: desc.to_string(),
            body: "…".to_string(),
            source: "~/.claude/skills".to_string(),
            source_path: None,
            base_dir: std::path::PathBuf::from("/home/me/.claude/skills").join(name),
            license: None,
            compatibility: None,
            metadata: Default::default(),
        }
    }

    #[test]
    fn source_styles_cover_all_row_kinds_without_changing_authored_text() {
        let root = std::path::PathBuf::from(r"/source/literal\name");
        let mut c = command("git/commit", r"description\with/slashes");
        c.source_path = Some(root.clone());
        let mut s = skill("normal", r"skill\description/text");
        s.source_path = Some(root.clone());
        let mut shadowed = s.clone();
        shadowed.name = "git/commit".into();
        let discovery = DiscoveredSkills {
            skills: vec![s.clone(), shadowed],
            shadowed: vec![s],
            invalid: vec![InvalidSkill {
                name: "broken".into(),
                path: "old file label".into(),
                source_path: Some(root.join("SKILL.md")),
                reason: r"reason\is/not/a/path".into(),
            }],
        };
        let mut prose = command("prose", "");
        prose.source = r"prose\is/not/a/path".into();
        let commands = vec![c, prose];
        let original = (commands.clone(), discovery.clone());
        for style in [false, true, false] {
            let rows = prompt_entries(&commands, &discovery, style);
            let expected = if cfg!(windows) {
                if style {
                    "/source/literal/name"
                } else {
                    r"\source\literal\name"
                }
            } else {
                r"/source/literal\name"
            };
            for index in [0, 2, 3, 4] {
                assert_eq!(rows[index].source, expected);
                assert_eq!(rows[index].source_path.as_ref(), Some(&root));
            }
            assert_eq!(rows[1].source, commands[1].source);
            assert_eq!(rows[1].detail(), commands[1].source);
            let sep = if cfg!(windows) && !style { '\\' } else { '/' };
            assert_eq!(rows[5].source, format!("{expected}{sep}SKILL.md"));
            assert_eq!(rows[5].detail(), r"invalid skill: reason\is/not/a/path");
            assert_eq!(rows[0].name, "git/commit");
            assert_eq!(rows[0].detail(), commands[0].description);
            assert_eq!(
                prompt_completions(":git/", &commands, &discovery.skills),
                vec![(":git/commit".into(), commands[0].description.clone())]
            );
            assert_eq!((&commands, &discovery), (&original.0, &original.1));
        }
    }

    #[test]
    fn completions_rank_prefix_then_substring_then_description() {
        let commands = vec![
            command("ship", "release checklist"),
            command("review", "inspect a shipped diff"),
        ];
        let names = |i: &str| {
            prompt_completions(i, &commands, &[])
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

    /// A namespaced command completes from its namespace, whichever separator
    /// the user reaches for — and the row offered is always the canonical `/`
    /// spelling, so accepting it inserts the name the listing shows.
    #[test]
    fn a_namespace_prefix_surfaces_its_nested_commands() {
        let commands = vec![
            command("git/commit", "commit the tree"),
            command("ship", "release checklist"),
        ];
        let names = |i: &str| {
            prompt_completions(i, &commands, &[])
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        };
        for typed in [":git", ":git/", ":git:", ":git.", ":GIT/co"] {
            assert_eq!(names(typed), vec![":git/commit"], "typed {typed}");
        }
    }

    /// Skills share the popup with commands — a `:` invocation is one namespace,
    /// so the completion list has to be one list. A shadowed skill contributes no
    /// row: its name already belongs to the command above it.
    #[test]
    fn completions_include_skills_and_skip_the_shadowed_ones() {
        let commands = vec![command("ship", "release checklist")];
        let skills = vec![
            skill("pdf-fill", "fill in a PDF form"),
            skill("ship", "the shadowed one"),
        ];
        let names = |i: &str| {
            prompt_completions(i, &commands, &skills)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(":"), vec![":pdf-fill", ":ship"]);
        assert_eq!(names(":pdf"), vec![":pdf-fill"]);
        // One `:ship` row, and it is the command's.
        assert_eq!(
            prompt_completions(":ship", &commands, &skills),
            vec![(":ship".to_string(), "release checklist".to_string())]
        );
    }

    /// The picker filter matches across name, detail and source — the fields the
    /// `/commands` rows show. The detail carries the kind, so `skill` narrows to
    /// the skills.
    #[test]
    fn filter_matches_name_detail_and_source() {
        let entries = prompt_entries(
            &[
                command("ship", "release checklist"),
                command("audit", "review"),
            ],
            &DiscoveredSkills {
                skills: vec![skill("pdf-fill", "fill in a PDF form")],
                ..Default::default()
            },
            true,
        );
        let hay = entries
            .iter()
            .map(prompt_entry_haystack)
            .collect::<Vec<_>>();
        let hits = |q: &str| filter_prompt_entries(&hay, q);
        assert_eq!(hits(""), vec![0, 1, 2]);
        assert_eq!(hits("ship"), vec![0]);
        assert_eq!(hits("checklist"), vec![0]);
        assert_eq!(hits("test"), vec![0, 1], "source matches");
        assert_eq!(hits("skill"), vec![2], "the kind is searchable");
        assert!(hits("nomatch").is_empty());
    }

    /// The picker's one flat view of the namespace: commands, then skills, with
    /// the shadowed and the invalid ones visible and labelled — "why is my skill
    /// not showing up" has to be answerable from this screen. Both shadowings
    /// are marked, and they are different situations: a command owning the name
    /// leaves the bundle loadable through the `skill` tool, while a
    /// higher-precedence skill root leaves nothing able to reach it at all.
    #[test]
    fn entries_mark_both_shadowings_and_invalid_skills() {
        let entries = prompt_entries(
            &[command("ship", "release checklist")],
            &DiscoveredSkills {
                skills: vec![
                    skill("ship", "the shadowed one"),
                    skill("pdf-fill", "fill a PDF"),
                ],
                shadowed: vec![skill("pdf-fill", "the user-scope copy")],
                invalid: vec![InvalidSkill {
                    name: "broken".to_string(),
                    path: "~/.claude/skills/broken/SKILL.md".to_string(),
                    source_path: None,
                    reason: "missing `description`".to_string(),
                }],
            },
            true,
        );
        let by = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
        assert_eq!(by("ship").kind, PromptEntryKind::Command);
        assert_eq!(by("ship").detail(), "release checklist");
        assert_eq!(by("pdf-fill").kind, PromptEntryKind::Skill);
        assert_eq!(by("pdf-fill").detail(), "skill · fill a PDF");
        // Both `ship` rows are present: the command's, and the skill it shadows —
        // which the model can still load through the `skill` tool.
        let details: Vec<String> = entries
            .iter()
            .filter(|e| matches!(e.kind, PromptEntryKind::ShadowedSkill(_)))
            .map(|e| format!("{}: {}", e.name, e.detail()))
            .collect();
        assert_eq!(
            details,
            vec![
                "ship: skill, shadowed by a command · the shadowed one",
                "pdf-fill: skill, shadowed by a higher-precedence skill · the user-scope copy",
            ],
            "both shadowings are shown, and say which one it is"
        );
        assert_eq!(
            by("broken").detail(),
            "invalid skill: missing `description`",
            "the reason is on the row"
        );
    }
}

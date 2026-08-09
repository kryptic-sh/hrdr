//! Frontend half of the `:name` prompt commands: the `:`-completion popup and
//! the `/commands` picker filter. (The `/name` slash commands are the rest of
//! this module.)
//!
//! Discovery, parsing and expansion live in [`hrdr_agent::commands`] — the model
//! can invoke a command through the `command` tool, so the agent owns that half and
//! both invocation paths expand through the same code. Re-exported here so the
//! frontends keep referring to `hrdr_app::Command` / `hrdr_app::discover_commands`.

pub use hrdr_agent::{
    Command, ProjectInstructions, builtin_commands, command_match_key, discover_commands,
    expand_command,
};

/// The lowercase haystack [`filter_commands`] matches against: the space-joined
/// `"name description source"`, precomputed once per picker open.
pub fn command_haystack(cmd: &Command) -> String {
    format!("{} {} {}", cmd.name, cmd.description, cmd.source).to_lowercase()
}

/// Case-insensitive fuzzy filter over precomputed command haystacks (built by
/// [`command_haystack`]): the query's characters must appear in order within the
/// haystack. Returns matching indices in input order; an empty query matches
/// everything.
pub fn filter_commands(haystacks: &[String], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..haystacks.len()).collect();
    }
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    haystacks
        .iter()
        .enumerate()
        .filter_map(|(i, hay)| hrdr_agent::fuzzy_match_hay(&q, hay).then_some(i))
        .collect()
}

/// Commands matching an in-progress `:…` input (empty once a space is typed) as
/// `(":name", description)` rows for the completion popup. Ranked like the
/// slash commands: name-prefix, then name-substring, then description.
///
/// Names are matched through [`command_match_key`], so a namespace typed with
/// any of its three separators (`:git/`, `:git:`, `:git.`) narrows to the same
/// rows — but the row inserted is always the canonical `/` spelling.
pub fn command_completions(input: &str, commands: &[Command]) -> Vec<(String, String)> {
    let Some(query) = input.strip_prefix(':') else {
        return Vec::new();
    };
    if query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let q = command_match_key(query);
    let mut scored: Vec<(u8, (String, String))> = Vec::new();
    for s in commands {
        let nl = command_match_key(&s.name);
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

    fn command(name: &str, desc: &str) -> Command {
        Command {
            name: name.to_string(),
            description: desc.to_string(),
            body: "…".to_string(),
            source: "test".to_string(),
            args: Vec::new(),
            model_invocable: true,
        }
    }

    #[test]
    fn completions_rank_prefix_then_substring_then_description() {
        let commands = vec![
            command("ship", "release checklist"),
            command("review", "inspect a shipped diff"),
        ];
        let names = |i: &str| {
            command_completions(i, &commands)
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
            command_completions(i, &commands)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        };
        for typed in [":git", ":git/", ":git:", ":git.", ":GIT/co"] {
            assert_eq!(names(typed), vec![":git/commit"], "typed {typed}");
        }
    }

    /// The picker filter matches across name, description and source — the
    /// fields the `/commands` rows show.
    #[test]
    fn filter_matches_name_description_and_source() {
        let commands = [
            command("ship", "release checklist"),
            command("audit", "review"),
        ];
        let hay = commands.iter().map(command_haystack).collect::<Vec<_>>();
        let hits = |q: &str| filter_commands(&hay, q);
        assert_eq!(hits(""), vec![0, 1]);
        assert_eq!(hits("ship"), vec![0]);
        assert_eq!(hits("checklist"), vec![0]);
        assert_eq!(hits("test"), vec![0, 1], "source matches");
        assert!(hits("nomatch").is_empty());
    }
}

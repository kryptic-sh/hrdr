//! Frontend half of skills: the `:`-completion popup and the `/skills` picker
//! filter.
//!
//! Discovery, parsing and expansion live in [`hrdr_agent::skills`] — the model
//! can invoke a skill through the `skill` tool, so the agent owns that half and
//! both invocation paths expand through the same code. Re-exported here so the
//! frontends keep referring to `hrdr_app::Skill` / `hrdr_app::discover_skills`.

pub use hrdr_agent::{ProjectInstructions, Skill, builtin_skills, discover_skills, expand_skill};

/// Case-insensitive fuzzy filter over skills for the `/skills` picker: the
/// query's characters must appear in order within `"name description source"`.
/// Returns matching indices in input order; an empty query matches everything.
pub fn filter_skills(skills: &[Skill], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..skills.len()).collect();
    }
    skills
        .iter()
        .enumerate()
        .filter_map(|(i, sk)| {
            hrdr_agent::fuzzy_match(
                query,
                &[
                    sk.name.as_str(),
                    sk.description.as_str(),
                    sk.source.as_str(),
                ],
            )
            .then_some(i)
        })
        .collect()
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

    fn skill(name: &str, desc: &str) -> Skill {
        Skill {
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
        let skills = vec![
            skill("ship", "release checklist"),
            skill("review", "inspect a shipped diff"),
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

    /// The picker filter matches across name, description and source — the
    /// fields the `/skills` rows show.
    #[test]
    fn filter_matches_name_description_and_source() {
        let skills = vec![skill("ship", "release checklist"), skill("audit", "review")];
        assert_eq!(filter_skills(&skills, ""), vec![0, 1]);
        assert_eq!(filter_skills(&skills, "ship"), vec![0]);
        assert_eq!(filter_skills(&skills, "checklist"), vec![0]);
        assert_eq!(filter_skills(&skills, "test"), vec![0, 1], "source matches");
        assert!(filter_skills(&skills, "nomatch").is_empty());
    }
}

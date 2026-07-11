//! The `/effort` picker's data: the reasoning-effort levels the current model
//! actually accepts (from the models.dev catalog's `reasoning_options`),
//! ordered highest effort first, with a leading "Default" row that clears the
//! override and lets the model/provider default apply. Pure over the cached
//! catalog so the list is testable without a network.

/// One pickable effort level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortChoice {
    /// The value sent as `reasoning_effort` (or mapped to a thinking budget on
    /// the native Anthropic backend). `None` = the "Default" row: clear the
    /// override and let the model/provider default apply.
    pub value: Option<String>,
    /// Human-readable label ("High", "Extra high", …).
    pub label: String,
    /// Short hint for the picker's second column.
    pub detail: String,
}

/// The known ladder, highest effort first: `(value, label, detail)`.
/// `sort rank` is the index — unknown catalog values keep their (reversed)
/// catalog position after the known ones.
const LADDER: &[(&str, &str, &str)] = &[
    ("max", "Max", "deepest reasoning"),
    ("xhigh", "Extra high", ""),
    ("high", "High", ""),
    ("medium", "Medium", ""),
    ("low", "Low", ""),
    ("minimal", "Minimal", "fastest"),
    ("none", "None", "reasoning off"),
];

/// The fallback offering when the catalog doesn't know the model (a local
/// server, a fresh cache): the four levels every backend maps.
const FALLBACK: &[&str] = &["minimal", "low", "medium", "high"];

/// Every effort the user can pick for `(provider, model)`: "Default" first,
/// then the model's own catalog-declared levels from highest to lowest (the
/// fallback ladder when the catalog doesn't carry the model). Reads the
/// models.dev catalog synchronously from cache — the picker builds its list on
/// a keypress and can't await a fetch.
pub fn effort_choices(provider: Option<&str>, model: &str) -> Vec<EffortChoice> {
    let values = hrdr_agent::catalog::load_cached()
        .and_then(|c| hrdr_agent::catalog::lookup_effort_levels(&c, provider, model))
        .unwrap_or_else(|| FALLBACK.iter().map(|v| (*v).to_string()).collect());
    choices_from(&values)
}

/// Build the choice list from a model's raw effort values. Pure, so ordering
/// and labeling are testable without a catalog cache.
pub fn choices_from(values: &[String]) -> Vec<EffortChoice> {
    let rank = |v: &str| LADDER.iter().position(|(id, _, _)| *id == v);
    let mut known: Vec<&String> = values.iter().filter(|v| rank(v).is_some()).collect();
    known.sort_by_key(|v| rank(v).unwrap());
    let mut out = vec![EffortChoice {
        value: None,
        label: "Default".to_string(),
        detail: "the model/provider default".to_string(),
    }];
    for v in known {
        let (_, label, detail) = LADDER.iter().find(|(id, _, _)| id == v).unwrap();
        out.push(EffortChoice {
            value: Some(v.clone()),
            label: (*label).to_string(),
            detail: (*detail).to_string(),
        });
    }
    // Catalog values outside the known ladder (future levels) still show,
    // highest-last catalog order reversed to match the descending list.
    for v in values.iter().rev().filter(|v| rank(v).is_none()) {
        let mut label: Vec<char> = v.chars().collect();
        if let Some(f) = label.first_mut() {
            *f = f.to_ascii_uppercase();
        }
        out.push(EffortChoice {
            value: Some(v.clone()),
            label: label.into_iter().collect(),
            detail: String::new(),
        });
    }
    out
}

/// Case-insensitive fuzzy filter over effort choices: the query's characters
/// must appear in order within `"label value detail"`. Returns matching
/// indices in input order; an empty query matches everything.
pub fn filter_effort_choices(choices: &[EffortChoice], query: &str) -> Vec<usize> {
    let q: Vec<char> = query.trim().to_lowercase().chars().collect();
    if q.is_empty() {
        return (0..choices.len()).collect();
    }
    choices
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let hay = format!(
                "{} {} {}",
                c.label,
                c.value.as_deref().unwrap_or("default"),
                c.detail
            )
            .to_lowercase();
            crate::is_subsequence(&q, &hay).then_some(i)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(list: &[&str]) -> Vec<String> {
        list.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn choices_order_highest_first_with_default_on_top() {
        // Catalog order is lowest-first; the picker shows highest-first.
        let c = choices_from(&vals(&["minimal", "low", "medium", "high"]));
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["Default", "High", "Medium", "Low", "Minimal"]);
        assert_eq!(c[0].value, None, "Default clears the override");
        assert_eq!(c[1].value.as_deref(), Some("high"));

        // The extended ladder (claude-fable-5 / codex-max style) orders too.
        let c = choices_from(&vals(&["low", "medium", "high", "xhigh", "max"]));
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Default", "Max", "Extra high", "High", "Medium", "Low"]
        );

        // An unknown future level still shows (capitalized), after the known.
        let c = choices_from(&vals(&["low", "ultra"]));
        assert_eq!(c.last().unwrap().label, "Ultra");
        assert_eq!(c.last().unwrap().value.as_deref(), Some("ultra"));
    }

    #[test]
    fn filter_matches_label_value_and_default() {
        let c = choices_from(&vals(&["minimal", "low", "medium", "high"]));
        let hits = |q: &str| filter_effort_choices(&c, q);
        assert_eq!(hits("").len(), c.len());
        // Subsequence match: "hig" hits High only ("hi" would also catch
        // Default via "the … provider").
        assert_eq!(hits("hig").len(), 1);
        assert_eq!(c[hits("hig")[0]].label, "High");
        // The Default row matches by its label and by "default".
        assert_eq!(c[hits("def")[0]].label, "Default");
        // Detail text matches too ("fastest" → Minimal).
        assert_eq!(c[hits("fastest")[0]].label, "Minimal");
        assert!(hits("zzz").is_empty());
    }
}

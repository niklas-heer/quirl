//! Terminal-independent typed fuzzy selection.

use serde::{Deserialize, Serialize};

pub const PICKER_PROTOCOL_VERSION: u32 = 1;
pub const PICKER_SCHEMA_DESCRIPTOR: &str = "quirl.picker@1{PickItem{deny_unknown;id:string;kind:history|file|directory|action|completion|job|data;label:string;description:string;preview:null|string;value:json};PickMatch{deny_unknown;index:usize;score:i32;match_indices:array<usize>};query:space-separated-AND,apostrophe-exact,bang-exclude;ordering:score-desc,label-asc,id-asc;selection:stable-index-into-input}";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    History,
    File,
    Directory,
    Action,
    Completion,
    Job,
    Data,
}

/// A display model that retains the original typed value in `value`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PickItem {
    pub id: String,
    pub kind: ItemKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PickMatch {
    pub index: usize,
    pub score: i32,
    pub match_indices: Vec<usize>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Picker;

impl Picker {
    /// Rank items deterministically. Space-separated terms are ANDed, a leading `'`
    /// requests an exact substring, and `!` excludes matching items.
    pub fn rank(&self, items: &[PickItem], query: &str) -> Vec<PickMatch> {
        let terms = query
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .collect::<Vec<_>>();
        let mut matches = items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| rank_item(item, &terms).map(|rank| (index, rank)))
            .map(|(index, (score, match_indices))| PickMatch {
                index,
                score,
                match_indices,
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| items[left.index].label.cmp(&items[right.index].label))
                .then_with(|| items[left.index].id.cmp(&items[right.index].id))
        });
        matches
    }

    pub fn select<'items>(
        &self,
        items: &'items [PickItem],
        query: &str,
        limit: usize,
    ) -> Vec<&'items PickItem> {
        self.rank(items, query)
            .into_iter()
            .take(limit)
            .map(|matched| &items[matched.index])
            .collect()
    }
}

fn rank_item(item: &PickItem, terms: &[&str]) -> Option<(i32, Vec<usize>)> {
    let searchable = if item.description.is_empty() {
        item.label.clone()
    } else {
        format!("{} {}", item.label, item.description)
    };
    let searchable_lower = searchable.to_lowercase();
    let mut score = 0;
    let mut primary_indices = Vec::new();
    for raw_term in terms {
        let (inverse, term) = raw_term
            .strip_prefix('!')
            .map_or((false, *raw_term), |term| (true, term));
        let (exact, term) = term
            .strip_prefix('\'')
            .map_or((false, term), |term| (true, term));
        if term.is_empty() {
            continue;
        }
        let term = term.to_lowercase();
        let matched = if exact {
            searchable_lower.find(&term).map(|start| {
                (
                    20_000 - i32::try_from(start).unwrap_or(i32::MAX),
                    (start..start + term.len()).collect::<Vec<_>>(),
                )
            })
        } else {
            fuzzy_match(&term, &searchable_lower)
        };
        if inverse {
            if matched.is_some() {
                return None;
            }
            continue;
        }
        let (term_score, indices) = matched?;
        score += term_score;
        if primary_indices.is_empty() {
            primary_indices = indices;
        }
    }
    Some((score, primary_indices))
}

fn fuzzy_match(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    if candidate.starts_with(query) {
        return Some((
            10_000 - i32::try_from(candidate.len()).unwrap_or(i32::MAX),
            (0..query.chars().count()).collect(),
        ));
    }
    let mut indices = Vec::new();
    let mut characters = candidate.char_indices();
    for wanted in query.chars() {
        let (byte_index, _) = characters.find(|(_, actual)| *actual == wanted)?;
        indices.push(candidate[..byte_index].chars().count());
    }
    let spread = i32::try_from(indices.last().copied().unwrap_or_default()).unwrap_or(i32::MAX);
    let length = i32::try_from(candidate.len()).unwrap_or(i32::MAX);
    Some((1_000 - spread - length, indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: ItemKind, label: &str) -> PickItem {
        PickItem {
            id: id.to_owned(),
            kind,
            label: label.to_owned(),
            description: String::new(),
            preview: None,
            value: serde_json::json!({ "original": id }),
        }
    }

    #[test]
    fn one_engine_ranks_history_files_actions_and_jobs_without_losing_values() {
        let items = vec![
            item("h1", ItemKind::History, "cargo test --workspace"),
            item("f1", ItemKind::File, "crates/quirl-core/src/lib.rs"),
            item("a1", ItemKind::Action, "Switch to data mode"),
            item("j1", ItemKind::Job, "deploy staging"),
        ];
        let selected = Picker.select(&items, "cts", 1);
        assert_eq!(selected[0].id, "h1");
        assert_eq!(selected[0].value["original"], "h1");
    }

    #[test]
    fn exact_inverse_and_multi_term_queries_are_deterministic() {
        let items = vec![
            item("1", ItemKind::File, "src/generated report.rs"),
            item("2", ItemKind::File, "src/final report.rs"),
            item("3", ItemKind::File, "docs/final report.md"),
        ];
        let selected = Picker.select(&items, "'final !docs", 10);
        assert_eq!(
            selected
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["2"]
        );
    }

    #[test]
    fn serialized_picker_contract_rejects_unknown_fields() {
        let source = r#"{"id":"1","kind":"file","label":"a","description":"","preview":null,"value":null,"future":true}"#;
        assert!(serde_json::from_str::<PickItem>(source).is_err());
        assert_eq!(PICKER_PROTOCOL_VERSION, 1);
        assert!(PICKER_SCHEMA_DESCRIPTOR.contains("selection:stable-index-into-input"));
    }
}

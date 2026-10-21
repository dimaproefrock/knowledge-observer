//! The observer engine: transcript tail, extraction contract, op apply, the
//! persistent observer agent, and the per-project daemon.
//!
//! This module root also hosts the pure **input-assembly** + **hint** helpers the
//! daemon's extraction pass relies on: `format_turns`, `dag_excerpt`,
//! `compute_index_delta`, `format_delta_hint`, `seed_baseline_datum`. They operate
//! on the **knowledge_dir-direct** store convention (the dir that holds `nodes/`).

pub mod agent;
pub mod apply;
pub mod contract;
pub mod daemon;
pub mod tail;

use std::path::Path;

use crate::store::knowledge_store::{KnowledgeGraph, Node};
use crate::store::observer_hint::HintState;

/// The seed baseline = the max `datum` over all nodes (ISO-8601 → lexicographic).
/// Empty graph → empty string (next pass seeds again, still emits nothing).
pub(crate) fn seed_baseline_datum(graph: &KnowledgeGraph) -> String {
    graph
        .nodes
        .iter()
        .map(|n| n.datum.as_str())
        .max()
        .unwrap_or("")
        .to_string()
}

/// Compute the Index-Delta: nodes that are status `active`/`supported`, whose `datum`
/// is strictly newer than the last-informed high-water mark, that did NOT originate
/// in THIS session (we never nudge the agent about its own turns), and that were not
/// already announced (dedupe by id). Pure — no I/O.
pub(crate) fn compute_index_delta<'a>(
    graph: &'a KnowledgeGraph,
    session_id: &str,
    state: &HintState,
) -> Vec<&'a Node> {
    graph
        .nodes
        .iter()
        .filter(|n| n.status == "active" || n.status == "supported")
        .filter(|n| n.datum.as_str() > state.last_informed_datum.as_str())
        .filter(|n| n.session_id.as_deref() != Some(session_id))
        .filter(|n| !state.informed_node_ids.iter().any(|id| id == &n.id))
        .collect()
}

/// One pointer line for an Index-Delta: count + up to 3 short titles (+ "…" if more).
pub(crate) fn format_delta_hint(delta: &[&Node]) -> String {
    const MAX_TITLES: usize = 3;
    let titles: Vec<String> = delta
        .iter()
        .take(MAX_TITLES)
        .map(|n| short_title(&n.inhalt))
        .collect();
    let mut joined = titles.join("; ");
    if delta.len() > MAX_TITLES {
        joined.push_str(" …");
    }
    format!(
        "[Knowledge] Index updated: {} new/changed node(s) — e.g. {}",
        delta.len(),
        joined
    )
}

/// Compact a node's `inhalt` to a short title (≤ 60 chars, ellipsis if cut).
fn short_title(inhalt: &str) -> String {
    let t = inhalt.trim();
    if t.chars().count() > 60 {
        t.chars().take(60).collect::<String>() + "…"
    } else {
        t.to_string()
    }
}

/// Robust-minimal turn formatting: pull `role` + the concatenated text blocks from
/// each transcript JSON line into "User:/Assistant:" prose. Lines that aren't the
/// expected shape fall back to the raw (stripped) line so nothing is silently lost.
pub(crate) fn format_turns(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                let role = v
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|r| r.as_str())
                    .or_else(|| v.get("type").and_then(|t| t.as_str()))
                    .unwrap_or("");
                let text = extract_text(&v);
                if text.trim().is_empty() {
                    continue;
                }
                let label = match role {
                    "user" => "User",
                    "assistant" => "Assistant",
                    other if !other.is_empty() => other,
                    _ => "?",
                };
                out.push_str(label);
                out.push_str(": ");
                out.push_str(text.trim());
                out.push_str("\n\n");
            }
            Err(_) => {
                // Not JSON (shouldn't happen for transcript lines) — keep verbatim.
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// Concatenate the human-readable text from a transcript line's `message.content`
/// (string, or array of `{type:"text",text}` / `{type:"tool_result",content}`).
fn extract_text(v: &serde_json::Value) -> String {
    let content = match v.get("message").and_then(|m| m.get("content")) {
        Some(c) => c,
        None => return String::new(),
    };
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut parts: Vec<String> = Vec::new();
            for block in arr {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                } else if let Some(c) = block.get("content").and_then(|c| c.as_str()) {
                    parts.push(c.to_string());
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// How many of this session's own newest nodes always go into the excerpt.
const EXCERPT_OWN_SESSION: usize = 10;

/// A compact, line-per-node excerpt (id · typ · status · titel · [tags]) for dedup
/// context. Reads + scores the graph from disk. `kdir` is the knowledge dir directly
/// (holds `nodes/`). Selection is TOPICAL, not by global score — in a large store the
/// observer must see the existing node it should UPDATE/replace, or it can only ADD:
/// this session's newest nodes, then the nodes matching `context` (delta + rolling
/// summary), then newest as filler. Superseded/retired nodes are left out.
pub(crate) fn dag_excerpt(kdir: &Path, limit: usize, context: &str, session_id: &str) -> String {
    let full = crate::store::knowledge_store::query(kdir);
    let mut out = String::new();
    for n in select_excerpt(&full, limit, context, session_id) {
        let titel: String = if n.inhalt.chars().count() > 100 {
            n.inhalt.chars().take(100).collect::<String>() + "…"
        } else {
            n.inhalt.clone()
        };
        out.push_str(&format!("- [{}] ({}/{}) {}", n.id, type_label(&n.typ), n.status, titel));
        if !n.tags.is_empty() {
            out.push_str(&format!("  tags: {}", n.tags.join(", ")));
        }
        out.push('\n');
    }
    out
}

/// Lowercased word tokens (≥ 4 chars) for the topical match.
fn tokens(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| t.chars().count() >= 4)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Pure selection behind `dag_excerpt`. Relevance = Σ idf of the context terms a node
/// contains (inhalt + tags), so frequent words weigh ~nothing without a stopword list.
fn select_excerpt<'a>(g: &'a KnowledgeGraph, limit: usize, context: &str, session_id: &str) -> Vec<&'a Node> {
    let live: Vec<&Node> = g
        .nodes
        .iter()
        .filter(|n| n.status != "superseded" && !n.ueberholt)
        .collect();
    let mut picked: Vec<&Node> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut take = |n: &'a Node, picked: &mut Vec<&'a Node>| {
        if picked.len() < limit && seen.insert(n.id.as_str()) {
            picked.push(n);
        }
    };

    // 1. This session's own newest nodes (`load` sorts ascending by datum).
    if !session_id.is_empty() {
        for n in live
            .iter()
            .rev()
            .filter(|n| n.session_id.as_deref() == Some(session_id))
            .take(EXCERPT_OWN_SESSION)
        {
            take(n, &mut picked);
        }
    }

    // 2. Topical match against the context.
    let ctx = tokens(context);
    let node_toks: Vec<std::collections::HashSet<String>> = live
        .iter()
        .map(|n| tokens(&format!("{} {}", n.inhalt, n.tags.join(" "))))
        .collect();
    let mut df: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for toks in &node_toks {
        for t in toks {
            if ctx.contains(t) {
                *df.entry(t.as_str()).or_insert(0) += 1;
            }
        }
    }
    let total = live.len().max(1) as f64;
    let mut scored: Vec<(f64, usize)> = node_toks
        .iter()
        .enumerate()
        .map(|(i, toks)| {
            let s: f64 = toks
                .iter()
                .filter_map(|t| df.get(t.as_str()))
                .map(|&d| (total / d as f64).ln())
                .sum();
            (s, i)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    // Ties → newer first (higher index = newer).
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.cmp(&a.1)));
    for (_, i) in scored {
        take(live[i], &mut picked);
    }

    // 3. Filler: newest nodes.
    for n in live.iter().rev() {
        take(n, &mut picked);
    }
    picked
}

/// Short label for a node type in the excerpt (Debug-format, lowercased).
fn type_label(typ: &crate::store::knowledge_store::NodeType) -> String {
    format!("{typ:?}").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::knowledge_store::NodeType;

    fn node(id: &str, status: &str, datum: &str, session: Option<&str>, inhalt: &str) -> Node {
        Node {
            id: id.to_string(),
            typ: NodeType::Fact,
            inhalt: inhalt.to_string(),
            begruendung: String::new(),
            datum: datum.to_string(),
            basis_score: 0.0,
            score: 0.0,
            status: status.to_string(),
            herkunft: "session".to_string(),
            frage_id: None,
            wert: None,
            quelle_ids: Vec::new(),
            session_id: session.map(|s| s.to_string()),
            tags: Vec::new(),
            ueberholt: false,
            erledigt: false,
        }
    }

    fn graph_of(nodes: Vec<Node>) -> KnowledgeGraph {
        KnowledgeGraph {
            nodes,
            ..Default::default()
        }
    }

    #[test]
    fn excerpt_prefers_own_session_then_topic_and_skips_superseded() {
        let mut old = node("old", "superseded", "2026-01-01", None, "Pathfinding über Diffusion");
        old.ueberholt = true;
        let g = graph_of(vec![
            old,
            node("topic", "active", "2026-01-02", None, "Pathfinding über Dijkstra im Voxelgitter"),
            node("noise", "active", "2026-01-03", None, "Buildserver nutzt Docker"),
            node("own", "active", "2026-01-04", Some("s1"), "Etwas ganz anderes"),
        ]);
        let ids: Vec<&str> = select_excerpt(&g, 2, "wie läuft das pathfinding im voxelgitter", "s1")
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(ids, vec!["own", "topic"]);
    }

    #[test]
    fn excerpt_falls_back_to_newest_without_topic_match() {
        let g = graph_of(vec![
            node("a", "active", "2026-01-01", None, "Alpha"),
            node("b", "active", "2026-01-02", None, "Beta"),
        ]);
        let ids: Vec<&str> = select_excerpt(&g, 1, "nichts passt", "").iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn format_turns_labels_roles_and_extracts_text() {
        let lines = vec![
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Use WAL mode"}]}}"#.to_string(),
            r#"{"type":"assistant","message":{"role":"assistant","content":"Done, WAL enabled"}}"#.to_string(),
        ];
        let out = format_turns(&lines);
        assert!(out.contains("User: Use WAL mode"));
        assert!(out.contains("Assistant: Done, WAL enabled"));
    }

    #[test]
    fn format_turns_skips_empty_text_lines() {
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{}}]}}"#.to_string(),
        ];
        let out = format_turns(&lines);
        assert!(out.trim().is_empty());
    }

    #[test]
    fn extract_text_handles_string_and_array_and_tool_result() {
        let s: serde_json::Value =
            serde_json::from_str(r#"{"message":{"content":"plain"}}"#).unwrap();
        assert_eq!(extract_text(&s), "plain");

        let a: serde_json::Value = serde_json::from_str(
            r#"{"message":{"content":[{"type":"text","text":"a"},{"type":"tool_result","content":"b"}]}}"#,
        )
        .unwrap();
        assert_eq!(extract_text(&a), "a\nb");
    }

    #[test]
    fn delta_includes_other_session_newer_node() {
        let g = graph_of(vec![node(
            "n1",
            "supported",
            "2026-06-17T12:00:00Z",
            Some("other-session"),
            "WAL mode",
        )]);
        let state = HintState {
            informed_node_ids: vec![],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
            last_feedback_at: 0,
        };
        let delta = compute_index_delta(&g, "this-session", &state);
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].id, "n1");
    }

    #[test]
    fn delta_excludes_own_session_node() {
        let g = graph_of(vec![node(
            "n1",
            "supported",
            "2026-06-17T12:00:00Z",
            Some("this-session"),
            "X",
        )]);
        let state = HintState {
            informed_node_ids: vec![],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
            last_feedback_at: 0,
        };
        assert!(compute_index_delta(&g, "this-session", &state).is_empty());
    }

    #[test]
    fn delta_excludes_retired_and_refuted_nodes() {
        let g = graph_of(vec![
            node("a", "superseded", "2026-06-17T12:00:00Z", Some("other"), "old"),
            node("b", "refuted", "2026-06-17T12:00:00Z", Some("other"), "wrong"),
            node("c", "done", "2026-06-17T12:00:00Z", Some("other"), "done"),
        ]);
        let state = HintState {
            informed_node_ids: vec![],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
            last_feedback_at: 0,
        };
        assert!(compute_index_delta(&g, "this-session", &state).is_empty());
    }

    #[test]
    fn delta_excludes_already_informed_id() {
        let g = graph_of(vec![node(
            "n1",
            "active",
            "2026-06-17T12:00:00Z",
            Some("other"),
            "X",
        )]);
        let state = HintState {
            informed_node_ids: vec!["n1".to_string()],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
            last_feedback_at: 0,
        };
        assert!(compute_index_delta(&g, "this-session", &state).is_empty());
    }

    #[test]
    fn delta_excludes_not_newer_than_watermark() {
        let g = graph_of(vec![node(
            "n1",
            "active",
            "2026-06-17T00:00:00Z",
            Some("other"),
            "X",
        )]);
        let state = HintState {
            informed_node_ids: vec![],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
            last_feedback_at: 0,
        };
        assert!(compute_index_delta(&g, "this-session", &state).is_empty());
    }

    #[test]
    fn seed_baseline_returns_max_datum() {
        let g = graph_of(vec![
            node("a", "active", "2026-06-10T00:00:00Z", Some("other"), "older"),
            node("b", "active", "2026-06-17T00:00:00Z", Some("other"), "newest"),
        ]);
        assert_eq!(seed_baseline_datum(&g), "2026-06-17T00:00:00Z");

        let seeded = HintState {
            informed_node_ids: vec![],
            last_informed_datum: seed_baseline_datum(&g),
            last_feedback_at: 0,
        };
        assert!(compute_index_delta(&g, "this-session", &seeded).is_empty());
    }

    #[test]
    fn seed_baseline_empty_graph_is_empty() {
        assert_eq!(seed_baseline_datum(&KnowledgeGraph::default()), "");
    }

    #[test]
    fn format_delta_hint_caps_titles_and_adds_ellipsis() {
        let nodes = vec![
            node("a", "active", "d", Some("o"), "Alpha"),
            node("b", "active", "d", Some("o"), "Beta"),
            node("c", "active", "d", Some("o"), "Gamma"),
            node("d", "active", "d", Some("o"), "Delta"),
        ];
        let refs: Vec<&Node> = nodes.iter().collect();
        let line = format_delta_hint(&refs);
        assert!(line.starts_with("[Knowledge] Index updated: 4 new/changed node(s)"));
        assert!(line.contains("Alpha"));
        assert!(line.contains("Beta"));
        assert!(line.contains("Gamma"));
        assert!(!line.contains("Delta"));
        assert!(line.contains("…"));
    }
}

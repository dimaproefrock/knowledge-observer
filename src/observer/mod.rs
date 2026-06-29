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

/// Compute the Index-Delta: nodes that are status `aktiv`/`gestützt`, whose `datum`
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
        .filter(|n| n.status == "aktiv" || n.status == "gestützt")
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
        "[Wissen] Index aktualisiert: {} neue/geänderte Knoten — z. B. {}",
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

/// A compact, line-per-node excerpt of the top-N scored nodes (id · typ · status ·
/// titel · [tags]) for dedup context. Reads + scores the graph from disk. `kdir` is
/// the knowledge dir directly (holds `nodes/`).
pub(crate) fn dag_excerpt(kdir: &Path, limit: usize) -> String {
    use crate::store::knowledge_store::{self as ks, QueryFilter};
    let full = ks::query(kdir);
    let filter = QueryFilter {
        limit: Some(limit),
        ..Default::default()
    };
    let view = ks::apply_filter(&full, &filter);
    let mut out = String::new();
    for n in &view.nodes {
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
            typ: NodeType::Fakt,
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
            "gestützt",
            "2026-06-17T12:00:00Z",
            Some("other-session"),
            "WAL mode",
        )]);
        let state = HintState {
            informed_node_ids: vec![],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
        };
        let delta = compute_index_delta(&g, "this-session", &state);
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].id, "n1");
    }

    #[test]
    fn delta_excludes_own_session_node() {
        let g = graph_of(vec![node(
            "n1",
            "gestützt",
            "2026-06-17T12:00:00Z",
            Some("this-session"),
            "X",
        )]);
        let state = HintState {
            informed_node_ids: vec![],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
        };
        assert!(compute_index_delta(&g, "this-session", &state).is_empty());
    }

    #[test]
    fn delta_excludes_retired_and_refuted_nodes() {
        let g = graph_of(vec![
            node("a", "überholt", "2026-06-17T12:00:00Z", Some("other"), "old"),
            node("b", "widerlegt", "2026-06-17T12:00:00Z", Some("other"), "wrong"),
            node("c", "erledigt", "2026-06-17T12:00:00Z", Some("other"), "done"),
        ]);
        let state = HintState {
            informed_node_ids: vec![],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
        };
        assert!(compute_index_delta(&g, "this-session", &state).is_empty());
    }

    #[test]
    fn delta_excludes_already_informed_id() {
        let g = graph_of(vec![node(
            "n1",
            "aktiv",
            "2026-06-17T12:00:00Z",
            Some("other"),
            "X",
        )]);
        let state = HintState {
            informed_node_ids: vec!["n1".to_string()],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
        };
        assert!(compute_index_delta(&g, "this-session", &state).is_empty());
    }

    #[test]
    fn delta_excludes_not_newer_than_watermark() {
        let g = graph_of(vec![node(
            "n1",
            "aktiv",
            "2026-06-17T00:00:00Z",
            Some("other"),
            "X",
        )]);
        let state = HintState {
            informed_node_ids: vec![],
            last_informed_datum: "2026-06-17T00:00:00Z".to_string(),
        };
        assert!(compute_index_delta(&g, "this-session", &state).is_empty());
    }

    #[test]
    fn seed_baseline_returns_max_datum() {
        let g = graph_of(vec![
            node("a", "aktiv", "2026-06-10T00:00:00Z", Some("other"), "older"),
            node("b", "aktiv", "2026-06-17T00:00:00Z", Some("other"), "newest"),
        ]);
        assert_eq!(seed_baseline_datum(&g), "2026-06-17T00:00:00Z");

        let seeded = HintState {
            informed_node_ids: vec![],
            last_informed_datum: seed_baseline_datum(&g),
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
            node("a", "aktiv", "d", Some("o"), "Alpha"),
            node("b", "aktiv", "d", Some("o"), "Beta"),
            node("c", "aktiv", "d", Some("o"), "Gamma"),
            node("d", "aktiv", "d", Some("o"), "Delta"),
        ];
        let refs: Vec<&Node> = nodes.iter().collect();
        let line = format_delta_hint(&refs);
        assert!(line.starts_with("[Wissen] Index aktualisiert: 4 neue/geänderte Knoten"));
        assert!(line.contains("Alpha"));
        assert!(line.contains("Beta"));
        assert!(line.contains("Gamma"));
        assert!(!line.contains("Delta"));
        assert!(line.contains("…"));
    }
}

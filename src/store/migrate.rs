//! One-time, idempotent migration of the legacy single-file `graph.json` store
//! into the per-node `.md` + sidecar-JSON layout.
//!
//! Mechanism: if `<knowledge_dir>/graph.json` exists and has not been converted
//! yet, parse it, write the new layout via `knowledge_store::save`-equivalent
//! primitives, then rename `graph.json` → `graph.json.converted` so it never runs
//! again. Best-effort: any error is logged and swallowed — a failed migration must
//! never break `load` (the store simply starts empty / from whatever already
//! exists in the new layout).

use std::path::Path;

use crate::store::knowledge_store::{atomic_write, serialize_node_md, KnowledgeGraph};

/// The legacy single-file store name.
const LEGACY: &str = "graph.json";
/// Marker: the legacy file renamed once converted. Its presence (or the absence of
/// `graph.json`) means migration already ran.
const CONVERTED: &str = "graph.json.converted";

/// Migrate `<knowledge_dir>/graph.json` into the `.md`/JSON layout if needed.
/// Idempotent and infallible (logs + continues on any error). Cheap when there's
/// nothing to do: a single `exists()` check on the legacy file.
pub fn migrate_if_needed(knowledge_dir: &Path) {
    let legacy = knowledge_dir.join(LEGACY);
    if !legacy.exists() {
        // Already converted (renamed away) or never existed → nothing to do.
        return;
    }
    if let Err(e) = convert(knowledge_dir, &legacy) {
        eprintln!("[migrate] migration of {legacy:?} failed (continuing empty): {e}");
    }
}

fn convert(knowledge_dir: &Path, legacy: &Path) -> Result<(), String> {
    let content = std::fs::read_to_string(legacy).map_err(|e| format!("read graph.json: {e}"))?;
    let graph: KnowledgeGraph =
        serde_json::from_str(&content).map_err(|e| format!("parse graph.json: {e}"))?;

    let nodes_dir = knowledge_dir.join("nodes");
    std::fs::create_dir_all(&nodes_dir).map_err(|e| format!("mkdir nodes: {e}"))?;

    for node in &graph.nodes {
        let path = nodes_dir.join(format!("{}.md", node.id));
        atomic_write(&path, serialize_node_md(node).as_bytes())
            .map_err(|e| format!("write node {}: {e}", node.id))?;
    }
    write_json(&knowledge_dir.join("edges.json"), &graph.edges)?;
    write_json(&knowledge_dir.join("fragen.json"), &graph.fragen)?;
    write_json(&knowledge_dir.join("quellen.json"), &graph.quellen)?;

    // Mark converted by renaming the legacy file so the migration never re-runs.
    let converted = knowledge_dir.join(CONVERTED);
    std::fs::rename(legacy, &converted).map_err(|e| format!("rename to .converted: {e}"))?;
    Ok(())
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value).map_err(|e| format!("serialize {path:?}: {e}"))?;
    atomic_write(path, json.as_bytes()).map_err(|e| format!("write {path:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::knowledge_store::{load_layout, Edge, KnowledgeGraph, Node, NodeType};

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ko-kmig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mk_node(id: &str) -> Node {
        Node {
            id: id.into(),
            typ: NodeType::Beobachtung,
            inhalt: "claude -p geht nicht".into(),
            begruendung: "warum: weil verifiziert".into(),
            datum: "2026-06-04T00:00:00Z".into(),
            basis_score: 0.9,
            score: 0.9,
            status: "gestützt".into(),
            herkunft: "session".into(),
            frage_id: None,
            wert: None,
            quelle_ids: vec![".claude/adr/1.md".into()],
            session_id: Some("sess-1".into()),
            tags: vec!["voxel".into()],
            ueberholt: false,
            erledigt: false,
        }
    }

    fn write_legacy(knowledge_dir: &Path, graph: &KnowledgeGraph) {
        std::fs::create_dir_all(knowledge_dir).unwrap();
        let json = serde_json::to_string_pretty(graph).unwrap();
        std::fs::write(knowledge_dir.join(LEGACY), json).unwrap();
    }

    #[test]
    fn migrates_legacy_graph_json_to_md_layout_and_marks_converted() {
        // The dir passed IS the knowledge dir (knowledge_dir-direct convention).
        let kdir = temp_dir();
        let graph = KnowledgeGraph {
            nodes: vec![mk_node("n1"), mk_node("n2")],
            edges: vec![Edge {
                id: "e1".into(),
                von: "n1".into(),
                zu: "n2".into(),
                polaritaet: "stuetzt".into(),
                gewicht: 1.0,
            }],
            fragen: vec![],
            quellen: vec![],
        };
        write_legacy(&kdir, &graph);

        migrate_if_needed(&kdir);

        // .md files written, sidecar present, legacy renamed.
        assert!(kdir.join("nodes").join("n1.md").exists());
        assert!(kdir.join("nodes").join("n2.md").exists());
        assert!(kdir.join("edges.json").exists());
        assert!(!kdir.join(LEGACY).exists(), "legacy graph.json renamed away");
        assert!(kdir.join(CONVERTED).exists(), "converted marker present");

        // Loading the new layout reproduces the graph.
        let loaded = load_layout(&kdir);
        assert_eq!(loaded.nodes.len(), 2);
        assert_eq!(loaded.edges.len(), 1);

        std::fs::remove_dir_all(&kdir).ok();
    }

    #[test]
    fn second_run_is_noop() {
        let kdir = temp_dir();
        write_legacy(&kdir, &KnowledgeGraph { nodes: vec![mk_node("n1")], ..Default::default() });

        migrate_if_needed(&kdir);
        // Capture the converted file's mtime/content, then run again.
        let converted = kdir.join(CONVERTED);
        let before = std::fs::read_to_string(&converted).unwrap();
        // Tamper a node file to prove the 2nd run does NOT overwrite it.
        let node_md = kdir.join("nodes").join("n1.md");
        std::fs::write(&node_md, "TAMPERED").unwrap();

        migrate_if_needed(&kdir);

        let after = std::fs::read_to_string(&converted).unwrap();
        assert_eq!(before, after, "converted marker unchanged");
        assert_eq!(std::fs::read_to_string(&node_md).unwrap(), "TAMPERED", "2nd run did not re-migrate");

        std::fs::remove_dir_all(&kdir).ok();
    }

    #[test]
    fn already_md_layout_is_untouched() {
        let kdir = temp_dir();
        let ndir = kdir.join("nodes");
        std::fs::create_dir_all(&ndir).unwrap();
        // A dir already in .md form (no graph.json).
        std::fs::write(ndir.join("x.md"), serialize_node_md(&mk_node("x"))).unwrap();
        let edges_path = kdir.join("edges.json");
        std::fs::write(&edges_path, "[]").unwrap();
        let edges_before = std::fs::read_to_string(&edges_path).unwrap();

        migrate_if_needed(&kdir);

        assert!(!kdir.join(CONVERTED).exists(), "no converted marker created");
        assert_eq!(std::fs::read_to_string(&edges_path).unwrap(), edges_before, "untouched");
        // The pre-existing node loads.
        let loaded = load_layout(&kdir);
        assert_eq!(loaded.nodes.len(), 1);
        assert_eq!(loaded.nodes[0].id, "x");

        std::fs::remove_dir_all(&kdir).ok();
    }
}

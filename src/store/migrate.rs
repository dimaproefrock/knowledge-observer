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

/// Marker dropped once the German→English code conversion has run, so the
/// (slightly more expensive) directory rewrite happens at most once per store.
const LANG_CONVERTED: &str = ".lang-en.converted";

/// Migrate `<knowledge_dir>/graph.json` into the `.md`/JSON layout if needed.
/// Idempotent and infallible (logs + continues on any error). Cheap when there's
/// nothing to do: a single `exists()` check on the legacy file.
///
/// Two phases, both idempotent:
/// 1. legacy `graph.json` → per-node `.md` + sidecar JSON (the original migration);
/// 2. one-time German→English value conversion of an existing `.md`/`edges.json`
///    store (node `typ:` codes + edge `polaritaet` values). Status is recomputed on
///    read, so once the engine emits English, stored statuses become English for
///    free — no stored-status migration is needed.
pub fn migrate_if_needed(knowledge_dir: &Path) {
    let legacy = knowledge_dir.join(LEGACY);
    if legacy.exists() {
        if let Err(e) = convert(knowledge_dir, &legacy) {
            eprintln!("[migrate] migration of {legacy:?} failed (continuing empty): {e}");
        }
    }
    // Phase 2: flip a legacy-German store to English codes (once).
    if let Err(e) = convert_lang_if_needed(knowledge_dir) {
        eprintln!("[migrate] German→English conversion failed (tolerant parse still loads): {e}");
    }
}

/// German node-type code → English code (the canonical serialized form).
fn type_de_to_en(code: &str) -> Option<&'static str> {
    match code {
        "entscheidung" => Some("decision"),
        "erkenntnis" => Some("insight"),
        "fakt" => Some("fact"),
        "beobachtung" => Some("observation"),
        "recherche" => Some("research"),
        "vermutung" => Some("hypothesis"),
        _ => None,
    }
}

/// German edge polarity → English polarity.
fn pol_de_to_en(code: &str) -> Option<&'static str> {
    match code {
        "stuetzt" | "stützt" => Some("supports"),
        "widerspricht" => Some("contradicts"),
        "ersetzt" => Some("replaces"),
        _ => None,
    }
}

/// One-time, marked German→English rewrite of an existing `.md`/`edges.json` store.
/// Detects German codes (a node `.md` with a German `typ:` value or an `edges.json`
/// with a German `polaritaet`), rewrites them in place, and drops a marker so it
/// runs once. No-op (and no marker) for an empty/non-existent/already-English store,
/// so a brand-new English store never carries the marker needlessly.
fn convert_lang_if_needed(knowledge_dir: &Path) -> Result<(), String> {
    if knowledge_dir.join(LANG_CONVERTED).exists() {
        return Ok(()); // already converted
    }

    let mut rewrote_any = false;

    // Nodes: rewrite the `typ:` frontmatter line German→English.
    let ndir = knowledge_dir.join("nodes");
    if let Ok(entries) = std::fs::read_dir(&ndir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else { continue };
            if let Some(new_content) = rewrite_typ_line(&content) {
                atomic_write(&path, new_content.as_bytes())
                    .map_err(|e| format!("rewrite node {path:?}: {e}"))?;
                rewrote_any = true;
            }
        }
    }

    // Edges: rewrite each `polaritaet` German→English.
    let edges_path = knowledge_dir.join("edges.json");
    if let Ok(content) = std::fs::read_to_string(&edges_path) {
        if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) {
            let mut changed = false;
            if let Some(arr) = val.as_array_mut() {
                for e in arr {
                    if let Some(pol) = e.get("polaritaet").and_then(|p| p.as_str()) {
                        if let Some(en) = pol_de_to_en(pol) {
                            e["polaritaet"] = serde_json::Value::String(en.to_string());
                            changed = true;
                        }
                    }
                }
            }
            if changed {
                let json = serde_json::to_string_pretty(&val)
                    .map_err(|e| format!("serialize edges: {e}"))?;
                atomic_write(&edges_path, json.as_bytes())
                    .map_err(|e| format!("rewrite edges: {e}"))?;
                rewrote_any = true;
            }
        }
    }

    // Only drop the marker when there was an actual store to convert (nodes dir
    // exists), so an empty store re-checks cheaply but a real conversion runs once.
    if rewrote_any || ndir.exists() {
        atomic_write(&knowledge_dir.join(LANG_CONVERTED), b"converted\n")
            .map_err(|e| format!("write lang marker: {e}"))?;
    }
    Ok(())
}

/// If the `.md` frontmatter holds a German `typ:` value, return the full file with
/// that one line rewritten to the English code; else `None` (nothing to do). The
/// `typ:` value is JSON-encoded (e.g. `typ: "fakt"`), matching `serialize_node_md`.
fn rewrite_typ_line(content: &str) -> Option<String> {
    let mut out = String::with_capacity(content.len());
    let mut changed = false;
    let mut in_frontmatter = false;
    let mut seen_open = false;
    for (i, line) in content.split_inclusive('\n').enumerate() {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if i == 0 && trimmed == "---" {
            in_frontmatter = true;
            seen_open = true;
            out.push_str(line);
            continue;
        }
        if in_frontmatter && trimmed == "---" {
            in_frontmatter = false;
            out.push_str(line);
            continue;
        }
        if in_frontmatter {
            if let Some(rest) = trimmed.strip_prefix("typ:") {
                let raw = rest.trim();
                // value may be JSON-quoted ("fakt") or bare (fakt)
                let code = serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string());
                if let Some(en) = type_de_to_en(code.trim().to_lowercase().as_str()) {
                    let nl = &line[trimmed.len()..]; // preserved line ending
                    out.push_str(&format!("typ: {}{}", serde_json::to_string(en).unwrap(), nl));
                    changed = true;
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    if seen_open && changed {
        Some(out)
    } else {
        None
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
            typ: NodeType::Observation,
            inhalt: "claude -p does not work".into(),
            begruendung: "why: verified by hand".into(),
            datum: "2026-06-04T00:00:00Z".into(),
            basis_score: 0.9,
            score: 0.9,
            status: "supported".into(),
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

    /// Section B: a store seeded with LEGACY GERMAN codes (German `typ:` values in
    /// `nodes/*.md`, German `polaritaet` in `edges.json`) must (a) still LOAD via the
    /// tolerant `NodeType::parse`, and (b) be FLIPPED to English codes on disk by the
    /// one-time conversion pass. Status is recomputed on read → English for free.
    #[test]
    fn legacy_german_store_loads_and_converts_to_english() {
        let kdir = temp_dir();
        let ndir = kdir.join("nodes");
        std::fs::create_dir_all(&ndir).unwrap();

        // Hand-write two German-coded node .md files (mirrors serialize_node_md, but
        // with German `typ:` values as an old store would have on disk).
        let german_node = |id: &str, typ_de: &str| -> String {
            format!(
                "---\nid: {id:?}\ntyp: {typ_de:?}\ndatum: \"2026-06-04T00:00:00Z\"\nbasis_score: 0.9\nscore: 0.9\nstatus: \"gestützt\"\nherkunft: \"session\"\nquelle_ids: []\ntags: []\nueberholt: false\nerledigt: false\n---\n\nstatement {id}\n\n<!-- begründung -->\nwhy {id}\n"
            )
        };
        std::fs::write(ndir.join("d1.md"), german_node("d1", "entscheidung")).unwrap();
        std::fs::write(ndir.join("e1.md"), german_node("e1", "erkenntnis")).unwrap();

        // German-coded edges.json.
        std::fs::write(
            kdir.join("edges.json"),
            r#"[{"id":"x","von":"d1","zu":"e1","polaritaet":"stuetzt","gewicht":1.0}]"#,
        )
        .unwrap();

        // load (runs migrate_if_needed internally).
        let g = crate::store::knowledge_store::load(&kdir);
        assert_eq!(g.nodes.len(), 2, "legacy German nodes must still load");
        assert!(g.nodes.iter().any(|n| n.id == "d1" && n.typ == NodeType::Decision));
        assert!(g.nodes.iter().any(|n| n.id == "e1" && n.typ == NodeType::Insight));
        assert_eq!(g.edges.len(), 1);

        // On disk: the node .md `typ:` values are now ENGLISH, and the edge polarity
        // is English too.
        let d1 = std::fs::read_to_string(ndir.join("d1.md")).unwrap();
        assert!(d1.contains("typ: \"decision\""), "node typ flipped to English: {d1}");
        assert!(!d1.contains("entscheidung"), "no German typ left: {d1}");
        let edges = std::fs::read_to_string(kdir.join("edges.json")).unwrap();
        assert!(edges.contains("supports"), "edge polarity flipped to English: {edges}");
        assert!(!edges.contains("stuetzt"), "no German polarity left: {edges}");

        // Status is recomputed on read → English.
        let g2 = crate::store::knowledge_store::query(&kdir);
        assert!(g2.nodes.iter().all(|n| n.status.is_ascii()), "statuses are English ascii");
        assert!(g2.nodes.iter().any(|n| n.id == "d1" && n.status == "active"));

        // Marker dropped → idempotent (2nd load does not error).
        assert!(kdir.join(".lang-en.converted").exists(), "lang marker present");
        let _ = crate::store::knowledge_store::load(&kdir);

        std::fs::remove_dir_all(&kdir).ok();
    }
}

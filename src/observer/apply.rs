//! In-process application of observer-extracted ops to the scored DAG.
//!
//! The observer's headless extraction produces an [`ExtractionResult`] (see
//! `contract.rs`); this module turns each [`Op`] into an [`IpcRequest`] and runs
//! it through [`crate::dispatch::dispatch_store`] — the **single** write path used
//! for all knowledge writes. Reusing `dispatch_store` means the observer inherits,
//! for free: the `begruendung`-required gate, dedup-on-record (a duplicate ADD is
//! idempotent, no second node), `resolve_ref` for links by id/title, cycle checks,
//! and **origin stamping** from `session_id`.
//!
//! Rust is the authority here: a malformed/garbage op produced by the LLM is
//! simply skipped, never trusted blindly.
//!
//! The Tauri `apply_ops` wrapper (which emitted a `knowledge-graph-changed` event
//! via an `AppHandle`) is dropped — the daemon/CLI host has no AppHandle. Callers
//! that want a change signal can use the returned mutation count.

use serde_json::json;

use crate::dispatch;
use crate::ipc::IpcRequest;
use crate::observer::contract::{ExtractionResult, Op};

/// Build an [`IpcRequest`] for a given store op + payload. `session_id` is set on
/// every request so `dispatch_store` stamps the recording node's origin.
fn ipc(project_dir: &str, session_id: &str, op: &str, payload: serde_json::Value) -> IpcRequest {
    IpcRequest {
        token: String::new(), // ignored by dispatch_store
        project_dir: project_dir.to_string(),
        op: op.to_string(),
        payload,
        session_id: session_id.to_string(),
    }
}

/// Apply the extracted ops to the project's DAG via `dispatch_store`
/// (origin = `session_id`). Returns the number of ops that mutated the graph.
///
/// Pure-ish and testable: no event emit happens here. `Noop` ops are skipped, and
/// an `Add` with an empty `begruendung` is dropped before dispatch (dispatch would
/// reject it anyway — we skip cleanly so it never counts as a mutation).
pub fn apply_ops_store(project_dir: &str, session_id: &str, result: &ExtractionResult) -> usize {
    use crate::config::Config;
    use crate::store::knowledge_store;
    use std::path::Path;

    let mut mutated = 0usize;

    // Document ids the LLM is allowed to cite. Any `quellen` id NOT in this set is
    // a hallucination/stale path → dropped (so a node never cites an unresolvable id).
    //
    // The scan base is the dir whose SIBLING folders hold the docs (skipping
    // `knowledge`) — i.e. `kdir.parent()`, resolved the same way `dispatch_store`
    // resolves it. For the default `.claude/knowledge` that is `.claude`.
    // TODO: this scan base could become its own config key later.
    let cfg = Config::resolve(Path::new(project_dir));
    let kdir = cfg.knowledge_dir_abs(Path::new(project_dir));
    let scan_dir = kdir.parent().map(Path::to_path_buf).unwrap_or(kdir);
    let known: std::collections::HashSet<String> = knowledge_store::scan_documents(&scan_dir)
        .into_iter()
        .map(|q| q.id)
        .collect();

    for op in &result.ops {
        let req = match op {
            Op::Noop => continue,

            Op::Add {
                typ,
                inhalt,
                begruendung,
                tags,
                links,
                quellen,
            } => {
                // dispatch_store rejects an empty begruendung; skip cleanly first.
                if begruendung.trim().is_empty() {
                    continue;
                }
                let links_json: Vec<serde_json::Value> = links
                    .iter()
                    .map(|l| {
                        json!({
                            "ziel": l.ziel,
                            "polaritaet": l.polaritaet,
                            "als": l.als,
                        })
                    })
                    .collect();
                let mut payload = json!({
                    "typ": typ,
                    "inhalt": inhalt,
                    "begruendung": begruendung,
                    "tags": tags,
                    "links": links_json,
                });
                // Cite only documents that actually exist (drop hallucinated ids).
                let cited = filter_known(quellen, &known);
                if !cited.is_empty() {
                    payload["quellen"] = json!(cited);
                }
                ipc(project_dir, session_id, "record", payload)
            }

            Op::Update { id, fields } => {
                let mut payload = serde_json::Map::new();
                payload.insert("id".to_string(), json!(id));
                if let Some(v) = &fields.inhalt {
                    payload.insert("inhalt".to_string(), json!(v));
                }
                if let Some(v) = &fields.begruendung {
                    payload.insert("begruendung".to_string(), json!(v));
                }
                if let Some(v) = &fields.typ {
                    payload.insert("typ".to_string(), json!(v));
                }
                if let Some(v) = &fields.tags {
                    payload.insert("tags".to_string(), json!(v));
                }
                if let Some(v) = fields.ueberholt {
                    payload.insert("ueberholt".to_string(), json!(v));
                }
                if let Some(v) = fields.erledigt {
                    payload.insert("erledigt".to_string(), json!(v));
                }
                if let Some(v) = &fields.quellen {
                    // Filter to real document ids (drop hallucinated/stale paths).
                    let cited = filter_known(v, &known);
                    payload.insert("quellen".to_string(), json!(cited));
                }
                ipc(project_dir, session_id, "update", serde_json::Value::Object(payload))
            }

            Op::Link { von, zu, polaritaet } => ipc(
                project_dir,
                session_id,
                "link",
                json!({ "von": von, "zu": zu, "polaritaet": polaritaet }),
            ),
        };

        let (resp, changed) = dispatch::dispatch_store(&req);
        if changed {
            mutated += 1;
        } else if !resp.ok {
            eprintln!(
                "[observer] op '{}' did not mutate: {}",
                req.op,
                resp.error.unwrap_or_default()
            );
        }
    }

    mutated
}

/// Keep only the ids present in `known`, preserving order. Hallucinated/stale
/// document ids are dropped — the safety net against citing unresolvable sources.
fn filter_known(ids: &[String], known: &std::collections::HashSet<String>) -> Vec<String> {
    ids.iter().filter(|id| known.contains(*id)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::observer::contract::{LinkSpec, UpdateFields};
    use crate::store::knowledge_store as ks;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("observer-apply-{}", uuid::Uuid::new_v4()))
    }

    /// Resolve the knowledge dir the same way `apply_ops_store`/`dispatch_store`
    /// do, so tests can read back what an op wrote.
    fn kdir(project_dir: &std::path::Path) -> std::path::PathBuf {
        let cfg = Config::resolve(project_dir);
        cfg.knowledge_dir_abs(project_dir)
    }

    /// The scan base used by `apply_ops_store` (kdir.parent()), where seeded docs
    /// must live to be discoverable.
    fn scan_dir(project_dir: &std::path::Path) -> std::path::PathBuf {
        kdir(project_dir).parent().unwrap().to_path_buf()
    }

    fn add(typ: &str, inhalt: &str, begruendung: &str) -> Op {
        Op::Add {
            typ: typ.to_string(),
            inhalt: inhalt.to_string(),
            begruendung: begruendung.to_string(),
            tags: vec![],
            links: vec![],
            quellen: vec![],
        }
    }

    /// Like `add`, but with cited document ids.
    fn add_with_quellen(typ: &str, inhalt: &str, begruendung: &str, quellen: Vec<&str>) -> Op {
        Op::Add {
            typ: typ.to_string(),
            inhalt: inhalt.to_string(),
            begruendung: begruendung.to_string(),
            tags: vec![],
            links: vec![],
            quellen: quellen.into_iter().map(str::to_string).collect(),
        }
    }

    /// Seed a real, scannable document under the scan base (the `.claude` dir),
    /// so `scan_documents` returns it; return its id (relative to the scan base's
    /// parent — i.e. the project root — matching `scan_documents`' path rule).
    fn seed_document(dir: &std::path::Path, rel_dir: &str, file: &str, body: &str) -> String {
        let folder = scan_dir(dir).join(rel_dir);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(file), body).unwrap();
        // `scan_documents` builds ids relative to scan_dir.parent() (the project
        // root). With the default `.claude/knowledge`, scan_dir = `<dir>/.claude`
        // and its parent is `<dir>`, so the id is ".claude/<rel_dir>/<file>".
        format!(".claude/{rel_dir}/{file}")
    }

    #[test]
    fn add_with_begruendung_lands_with_origin() {
        let dir = tmp();
        let result = ExtractionResult {
            ops: vec![add("beobachtung", "the pty manager owns the writer", "welche Frage?")],
            rolling_summary: String::new(),
            hint: String::new(),
        };
        let mutated = apply_ops_store(&dir.to_string_lossy(), "sess-77", &result);
        assert_eq!(mutated, 1);

        let g = ks::query(&kdir(&dir));
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].inhalt, "the pty manager owns the writer");
        assert_eq!(g.nodes[0].session_id.as_deref(), Some("sess-77"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_without_begruendung_is_dropped() {
        let dir = tmp();
        let result = ExtractionResult {
            ops: vec![add("beobachtung", "no rationale here", "   ")],
            rolling_summary: String::new(),
            hint: String::new(),
        };
        let mutated = apply_ops_store(&dir.to_string_lossy(), "sess-1", &result);
        assert_eq!(mutated, 0);

        let g = ks::query(&kdir(&dir));
        assert!(g.nodes.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_adds_create_only_one_node() {
        let dir = tmp();
        let result = ExtractionResult {
            ops: vec![
                add("fakt", "DB uses WAL mode", "user said"),
                add("fakt", "DB uses WAL mode", "user said"),
            ],
            rolling_summary: String::new(),
            hint: String::new(),
        };
        let mutated = apply_ops_store(&dir.to_string_lossy(), "sess-2", &result);
        // First add mutates; the second hits dedup → idempotent (no mutation).
        assert_eq!(mutated, 1);

        let g = ks::query(&kdir(&dir));
        assert_eq!(g.nodes.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn link_between_existing_nodes_creates_edge() {
        let dir = tmp();
        let proj = dir.to_string_lossy().to_string();

        // Create two nodes first via two Adds.
        let setup = ExtractionResult {
            ops: vec![
                add("erkenntnis", "Pathfinding is the bottleneck", "warum?"),
                add("beobachtung", "Profiler shows A* hot in frames", "welche Frage?"),
            ],
            rolling_summary: String::new(),
            hint: String::new(),
        };
        assert_eq!(apply_ops_store(&proj, "sess-3", &setup), 2);

        // Now link them by title substring.
        let link = ExtractionResult {
            ops: vec![Op::Link {
                von: "Pathfinding is the bottleneck".to_string(),
                zu: "Profiler shows A*".to_string(),
                polaritaet: "stuetzt".to_string(),
            }],
            rolling_summary: String::new(),
            hint: String::new(),
        };
        let mutated = apply_ops_store(&proj, "sess-3", &link);
        assert_eq!(mutated, 1);

        let g = ks::query(&kdir(&dir));
        assert_eq!(g.edges.len(), 1, "expected one edge between the two nodes");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_citing_real_document_lands_quelle_id() {
        let dir = tmp();
        let proj = dir.to_string_lossy().to_string();
        let doc_id = seed_document(&dir, "features", "F004-weapons.md", "# Weapons\nspec");
        // sanity: the document is actually discoverable as a Quelle with this id.
        let scanned = ks::scan_documents(&scan_dir(&dir));
        assert!(scanned.iter().any(|q| q.id == doc_id), "document not scanned");

        let result = ExtractionResult {
            ops: vec![add_with_quellen(
                "fakt",
                "Weapons use a 32-byte header",
                "documented in F004",
                vec![&doc_id],
            )],
            rolling_summary: String::new(),
            hint: String::new(),
        };
        let mutated = apply_ops_store(&proj, "sess-q1", &result);
        assert_eq!(mutated, 1);

        let g = ks::query(&kdir(&dir));
        assert_eq!(g.nodes.len(), 1);
        assert!(
            g.nodes[0].quelle_ids.contains(&doc_id),
            "expected node to cite {doc_id}, got {:?}",
            g.nodes[0].quelle_ids
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_citing_bogus_document_is_filtered_out() {
        let dir = tmp();
        let proj = dir.to_string_lossy().to_string();
        // Seed one real doc so the known-set is non-empty, then cite a different id.
        seed_document(&dir, "features", "real.md", "# Real");

        let result = ExtractionResult {
            ops: vec![add_with_quellen(
                "fakt",
                "Some fact",
                "user said",
                vec![".claude/features/does-not-exist.md"],
            )],
            rolling_summary: String::new(),
            hint: String::new(),
        };
        let mutated = apply_ops_store(&proj, "sess-q2", &result);
        assert_eq!(mutated, 1);

        let g = ks::query(&kdir(&dir));
        assert_eq!(g.nodes.len(), 1);
        assert!(
            g.nodes[0].quelle_ids.is_empty(),
            "bogus id should have been filtered, got {:?}",
            g.nodes[0].quelle_ids
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_nonexistent_node_does_not_mutate() {
        // Touches the imported contract types so the surface stays in sync; an
        // Update targeting a missing node simply doesn't mutate.
        let dir = tmp();
        let _ls = LinkSpec {
            ziel: "x".into(),
            polaritaet: "stuetzt".into(),
            als: "parent".into(),
        };
        let result = ExtractionResult {
            ops: vec![Op::Update {
                id: "does-not-exist".to_string(),
                fields: UpdateFields {
                    inhalt: Some("new".into()),
                    ueberholt: Some(true),
                    ..Default::default()
                },
            }],
            rolling_summary: String::new(),
            hint: String::new(),
        };
        let mutated = apply_ops_store(&dir.to_string_lossy(), "sess-4", &result);
        assert_eq!(mutated, 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}

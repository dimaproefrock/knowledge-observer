//! Op dispatch to the store (record/update/link/merge/query/...). The
//! AppHandle-free path: only the testable store half is kept (any host-specific
//! event wrapper is the host's responsibility).
//!
//! `IpcRequest`/`IpcResponse` are the wire contract for the daemon's IPC; they live
//! in [`crate::ipc`] and are re-used here unchanged.
//!
//! ## `knowledge_dir` resolution
//! The store takes the knowledge dir DIRECTLY, so here we resolve it via
//! [`crate::config::Config`]: the per-project config decides `knowledge_dir`
//! (default `.claude/knowledge`), and `knowledge_dir_abs` makes it absolute against
//! the project root. Document scanning gets `kdir.parent()` — the surrounding
//! project-meta dir whose sibling folders hold the docs.

use serde_json::{json, Value};

use crate::config::Config;
use crate::ipc::{IpcRequest, IpcResponse};
use crate::store::knowledge_store::{self as ks, NodeType};

/// The store half of dispatch — no GUI/event dependency, so it is directly
/// testable. Returns the response and whether it mutated the graph (the Tauri
/// wrapper used the bool to decide whether to emit `knowledge-graph-changed`;
/// here the daemon/CLI host can do likewise, or ignore it).
pub fn dispatch_store(req: &IpcRequest) -> (IpcResponse, bool) {
    let project_dir = std::path::Path::new(&req.project_dir);
    let cfg = Config::resolve(project_dir);
    let kdir = cfg.knowledge_dir_abs(project_dir);
    // Document scan base: the dir whose SIBLING folders hold the docs (skipping
    // `knowledge`). For the default `.claude/knowledge` that is `.claude`.
    // TODO: this scan base could become its own config key later.
    let scan_dir = kdir.parent().unwrap_or(&kdir).to_path_buf();
    let p = &req.payload;

    // Per-project gate. `ping` stays
    // reachable so callers can health-check even when knowledge is off.
    if !cfg.enabled && req.op != "ping" {
        return (IpcResponse::err("knowledge is disabled for this project"), false);
    }

    // `ks::query`/`ks::load` already run `migrate_if_needed` internally, so no
    // explicit migration call is needed before the first read.
    match req.op.as_str() {
        "ping" => (
            IpcResponse::ok(json!({
                "pong": true,
                "project": req.project_dir,
                "via": "knowledge-observer",
            })),
            false,
        ),

        // Compact overview / search: id + typ + status + score + short title + tags
        // (NO begruendung, truncated inhalt) so it stays cheap even for big graphs.
        // Fetch full detail for chosen ids via the `get` op.
        "query" => {
            let full = ks::query(&kdir);
            let mut hints = ks::hygiene_hints(&full);
            let filter = parse_query_filter(p);
            let view = ks::apply_filter(&full, &filter);
            if filter.is_active() && view.nodes.len() < full.nodes.len() {
                hints.insert(
                    0,
                    format!(
                        "Gefiltert: {} von {} Knoten. Filter: q/typ/status/tags/limit.",
                        view.nodes.len(),
                        full.nodes.len()
                    ),
                );
            }
            hints.push(
                "Kompakter Überblick (id+typ+titel). Volldetails (begruendung/quellen/inhalt) je Knoten via get_knowledge(ids:[...])."
                    .to_string(),
            );
            let mut v = compact_graph(&view);
            if let Some(o) = v.as_object_mut() {
                o.insert("hints".to_string(), json!(hints));
            }
            (IpcResponse::ok(v), false)
        }

        // Deep fetch by id: full nodes for the given ids + their neighbourhood.
        "get" => {
            let ids = parse_str_array(p, "ids").unwrap_or_default();
            if ids.is_empty() {
                return (
                    IpcResponse::err("get: requires 'ids' (node ids from query_knowledge)"),
                    false,
                );
            }
            let depth = p.get("depth").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(1);
            let full = ks::query(&kdir);
            let view = ks::get_subgraph(&full, &ids, depth);
            (IpcResponse::ok(serde_json::to_value(&view).unwrap_or_else(|_| json!({}))), false)
        }

        "record" => {
            let Some(typ) = p.get("typ").and_then(Value::as_str).and_then(NodeType::parse) else {
                return (
                    IpcResponse::err(
                        "record: missing/invalid 'typ' (entscheidung|erkenntnis|fakt|beobachtung|recherche|vermutung)",
                    ),
                    false,
                );
            };
            let Some(inhalt) = p.get("inhalt").and_then(Value::as_str) else {
                return (IpcResponse::err("record: missing 'inhalt'"), false);
            };
            let Some(begruendung) = nonempty(p, "begruendung") else {
                return (
                    IpcResponse::err(
                        "record: missing 'begruendung' — give the WHY: the question it answers (beobachtung/recherche/vermutung), the rationale (erkenntnis), the purpose 'damit/wozu' (entscheidung), or the source/where-from (fakt)",
                    ),
                    false,
                );
            };
            let basis = p.get("basis_score").and_then(Value::as_f64);
            // Dedup-on-record: a same-type node with an identical statement already
            // exists → idempotent, don't duplicate. Point the caller at it.
            let full = ks::query(&kdir);
            if let Some(dup_id) = ks::find_duplicate(&full, typ, inhalt) {
                let node = full.nodes.iter().find(|n| n.id == dup_id).cloned();
                let data = json!({
                    "recorded": node,
                    "duplicate_of": dup_id,
                    "note": "already recorded (same statement) — not duplicated; use update_knowledge to change it or link to it",
                    "hints": ks::hygiene_hints(&full),
                });
                return (IpcResponse::ok(data), false);
            }
            match ks::add_node(&kdir, typ, inhalt.to_string(), begruendung, basis, "session") {
                Ok(n) => {
                    let (q, tags) = (parse_quellen(p), parse_str_array(p, "tags"));
                    if q.is_some() || tags.is_some() {
                        let _ = ks::update_node(&kdir, &n.id, None, None, None, None, q, tags, None, None);
                    }
                    stamp_origin(&kdir, &n.id, req);
                    let links = apply_inline_links(&kdir, &n.id, p);
                    // Re-fetch so the returned node reflects the freshly added inline
                    // links (score/status recomputed), not the pre-link snapshot.
                    let fresh =
                        ks::query(&kdir).nodes.into_iter().find(|x| x.id == n.id).unwrap_or(n);
                    let mut resp = recorded_resp(&kdir, &fresh);
                    if !links.is_empty() {
                        if let Some(o) = resp.data.as_object_mut() {
                            o.insert("links".to_string(), json!(links));
                        }
                    }
                    (resp, true)
                }
                Err(e) => (IpcResponse::err(e.to_string()), false),
            }
        }

        "link" => {
            let (Some(von_ref), Some(zu_ref), Some(pol)) = (
                p.get("von").and_then(Value::as_str),
                p.get("zu").and_then(Value::as_str),
                p.get("polaritaet").and_then(Value::as_str),
            ) else {
                return (
                    IpcResponse::err("link: requires 'von', 'zu', 'polaritaet'"),
                    false,
                );
            };
            let gew = p.get("gewicht").and_then(Value::as_f64);
            let graph = ks::query(&kdir);
            let von = match ks::resolve_ref(&graph, von_ref) {
                Ok(x) => x,
                Err(e) => return (IpcResponse::err(format!("link 'von': {e}")), false),
            };
            let zu = match ks::resolve_ref(&graph, zu_ref) {
                Ok(x) => x,
                Err(e) => return (IpcResponse::err(format!("link 'zu': {e}")), false),
            };
            match ks::add_edge(&kdir, &von, &zu, pol, gew) {
                Ok(e) => (recorded_resp(&kdir, &e), true),
                Err(e) => (IpcResponse::err(e.to_string()), false),
            }
        }

        "merge" => {
            let (Some(from), Some(into)) = (nonempty(p, "from"), nonempty(p, "into")) else {
                return (IpcResponse::err("merge: requires 'from' and 'into'"), false);
            };
            let graph = ks::query(&kdir);
            let from_id = match ks::resolve_ref(&graph, from) {
                Ok(x) => x,
                Err(e) => return (IpcResponse::err(format!("merge 'from': {e}")), false),
            };
            let into_id = match ks::resolve_ref(&graph, into) {
                Ok(x) => x,
                Err(e) => return (IpcResponse::err(format!("merge 'into': {e}")), false),
            };
            match ks::merge_nodes(&kdir, &from_id, &into_id) {
                Ok(n) => (recorded_resp(&kdir, &n), true),
                Err(e) => (IpcResponse::err(e.to_string()), false),
            }
        }

        "add_fact" => {
            let (Some(frage), Some(wert), Some(inhalt)) = (
                p.get("frage").and_then(Value::as_str),
                p.get("wert").and_then(Value::as_str),
                p.get("inhalt").and_then(Value::as_str),
            ) else {
                return (
                    IpcResponse::err("add_fact: requires 'frage', 'wert', 'inhalt'"),
                    false,
                );
            };
            let Some(begruendung) = nonempty(p, "begruendung") else {
                return (
                    IpcResponse::err("add_fact: missing 'begruendung' — the source / where this fact comes from"),
                    false,
                );
            };
            let basis = p.get("basis_score").and_then(Value::as_f64);
            match ks::add_fact(&kdir, frage, wert, inhalt.to_string(), begruendung, basis, "session") {
                Ok(n) => {
                    let (q, tags) = (parse_quellen(p), parse_str_array(p, "tags"));
                    if q.is_some() || tags.is_some() {
                        let _ = ks::update_node(&kdir, &n.id, None, None, None, None, q, tags, None, None);
                    }
                    stamp_origin(&kdir, &n.id, req);
                    (recorded_resp(&kdir, &n), true)
                }
                Err(e) => (IpcResponse::err(e.to_string()), false),
            }
        }

        "list_documents" => {
            let docs = ks::scan_documents(&scan_dir);
            let _ = ks::set_quellen(&kdir, docs.clone());
            (IpcResponse::ok(json!({ "documents": docs })), false)
        }

        // Correct/retire an existing node (Claude fixing its own knowledge).
        "update" => {
            let Some(node_id) = nonempty(p, "id") else {
                return (IpcResponse::err("update: requires 'id' (from query_knowledge)"), false);
            };
            let typ = match p.get("typ").and_then(Value::as_str) {
                Some(t) => match NodeType::parse(t) {
                    Some(parsed) => Some(parsed),
                    None => return (IpcResponse::err(format!("update: invalid typ '{t}'")), false),
                },
                None => None,
            };
            let inhalt = p.get("inhalt").and_then(Value::as_str).map(str::to_string);
            let begruendung = nonempty(p, "begruendung").map(str::to_string);
            let basis = p.get("basis_score").and_then(Value::as_f64);
            let quellen = parse_str_array(p, "quellen");
            let tags = parse_str_array(p, "tags");
            let ueberholt = p.get("ueberholt").and_then(Value::as_bool);
            let erledigt = p.get("erledigt").and_then(Value::as_bool);
            match ks::update_node(&kdir, node_id, inhalt, begruendung, typ, basis, quellen, tags, ueberholt, erledigt) {
                Ok(n) => (recorded_resp(&kdir, &n), true),
                Err(e) => (IpcResponse::err(e.to_string()), false),
            }
        }

        // "What holds now": active (non-superseded) decisions + open questions.
        "current_state" => {
            let g = ks::query(&kdir);
            let decisions: Vec<Value> = g
                .nodes
                .iter()
                .filter(|n| matches!(n.typ, NodeType::Entscheidung) && n.status == "aktiv")
                .map(|n| json!({ "id": n.id, "inhalt": n.inhalt, "tags": n.tags }))
                .collect();
            // A question is "answered" once one value clearly leads (its fakt is
            // gestützt) — only genuinely contested/empty ones count as open.
            let mut offen: Vec<Value> = Vec::new();
            let mut beantwortet: Vec<Value> = Vec::new();
            for fr in &g.fragen {
                let mut best: Option<(&str, f64, &str)> = None; // (wert, score, status)
                for n in &g.nodes {
                    if n.frage_id.as_deref() == Some(fr.id.as_str()) {
                        if let Some(w) = &n.wert {
                            if best.map_or(true, |(_, s, _)| n.score > s) {
                                best = Some((w, n.score, n.status.as_str()));
                            }
                        }
                    }
                }
                let answered = matches!(best, Some((_, _, st)) if st == "gestützt");
                let entry = json!({
                    "id": fr.id, "frage": fr.inhalt, "werte": fr.werte,
                    "fuehrend": best.map(|(w, _, _)| w),
                });
                if answered {
                    beantwortet.push(entry);
                } else {
                    offen.push(entry);
                }
            }
            (
                IpcResponse::ok(json!({
                    "aktive_entscheidungen": decisions,
                    "offene_fragen": offen,
                    "beantwortete_fragen": beantwortet,
                })),
                false,
            )
        }

        other => (IpcResponse::err(format!("unknown op: {other}")), false),
    }
}

/// Parse the optional `query_knowledge` filter args from the request payload.
fn parse_query_filter(p: &Value) -> ks::QueryFilter {
    ks::QueryFilter {
        q: p.get("q").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string),
        typ: p.get("typ").and_then(Value::as_str).and_then(NodeType::parse),
        status: p
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string),
        limit: p.get("limit").and_then(Value::as_u64).map(|n| n as usize),
        tags: parse_str_array(p, "tags").unwrap_or_default(),
    }
}

/// Project a (filtered) graph to the compact overview: per node id/typ/status/
/// score/short title/tags (+ wert), plus compact edges and fragen. Drops
/// begruendung and truncates inhalt — the token-cheap "what exists" view.
fn compact_graph(g: &ks::KnowledgeGraph) -> Value {
    let nodes: Vec<Value> = g
        .nodes
        .iter()
        .map(|n| {
            let titel = if n.inhalt.chars().count() > 100 {
                format!("{}…", n.inhalt.chars().take(100).collect::<String>())
            } else {
                n.inhalt.clone()
            };
            let mut o = json!({
                "id": n.id,
                "typ": n.typ,
                "status": n.status,
                "score": n.score,
                "titel": titel,
            });
            if !n.tags.is_empty() {
                o["tags"] = json!(n.tags);
            }
            if let Some(w) = &n.wert {
                o["wert"] = json!(w);
            }
            o
        })
        .collect();
    let edges: Vec<Value> = g
        .edges
        .iter()
        .map(|e| json!({ "von": e.von, "zu": e.zu, "polaritaet": e.polaritaet }))
        .collect();
    json!({ "nodes": nodes, "edges": edges, "fragen": g.fragen })
}

/// Create the inline `links` from a record call: each `{ziel, polaritaet, als}`
/// connects the new node to `ziel` (resolved by id or unique title). `als`:
/// "parent" (default) → new -pol-> ziel; "child" → ziel -pol-> new. Returns a
/// per-link result line so the caller sees failures without a separate round-trip.
fn apply_inline_links(kdir: &std::path::Path, new_id: &str, p: &Value) -> Vec<String> {
    let Some(arr) = p.get("links").and_then(|v| v.as_array()) else {
        return vec![];
    };
    let graph = ks::query(kdir);
    let mut msgs = Vec::new();
    for l in arr {
        let ziel = l.get("ziel").and_then(Value::as_str).unwrap_or("").trim();
        if ziel.is_empty() {
            msgs.push("link skipped: missing 'ziel'".to_string());
            continue;
        }
        let pol = l.get("polaritaet").and_then(Value::as_str).unwrap_or("stuetzt");
        let als = l.get("als").and_then(Value::as_str).unwrap_or("parent");
        match ks::resolve_ref(&graph, ziel) {
            Ok(zid) => {
                let (von, zu) = if als == "child" {
                    (zid.as_str(), new_id)
                } else {
                    (new_id, zid.as_str())
                };
                match ks::add_edge(kdir, von, zu, pol, None) {
                    Ok(_) => msgs.push(format!("linked {von} -{pol}-> {zu}")),
                    Err(e) => msgs.push(format!("link to '{ziel}' failed: {e}")),
                }
            }
            Err(e) => msgs.push(format!("link to '{ziel}' failed: {e}")),
        }
    }
    msgs
}

/// Parse an optional string-array payload field (None if absent/all-empty).
fn parse_str_array(p: &Value, key: &str) -> Option<Vec<String>> {
    let arr = p.get(key)?.as_array()?;
    let v: Vec<String> = arr
        .iter()
        .filter_map(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// A required string payload field: present and non-empty after trimming, else None.
fn nonempty<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// Stamp the recording node with its origin session (from the IPC request, set by
/// the per-session MCP server). Best-effort provenance — never blocks the record.
fn stamp_origin(kdir: &std::path::Path, node_id: &str, req: &IpcRequest) {
    if !req.session_id.trim().is_empty() {
        let _ = ks::set_origin_session(kdir, node_id, req.session_id.trim());
    }
}

/// Optional `quellen` payload: an array of document paths/ids to cite (None if absent/empty).
fn parse_quellen(p: &Value) -> Option<Vec<String>> {
    let arr = p.get("quellen")?.as_array()?;
    let v: Vec<String> = arr
        .iter()
        .filter_map(|x| x.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .collect();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Wrap a freshly created entity together with current hygiene hints, so Claude
/// gets a tidy-up signal right after writing (Approach A).
fn recorded_resp<T: serde::Serialize>(kdir: &std::path::Path, entity: &T) -> IpcResponse {
    let hints = ks::hygiene_hints(&ks::query(kdir));
    let recorded = serde_json::to_value(entity).unwrap_or(Value::Null);
    IpcResponse::ok(json!({ "recorded": recorded, "hints": hints }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::knowledge_store as ks;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("observer-kdispatch-{}", uuid::Uuid::new_v4()))
    }

    /// Resolve the knowledge dir the same way `dispatch_store` does, so tests can
    /// read back what an op wrote.
    fn kdir(project_dir: &std::path::Path) -> std::path::PathBuf {
        let cfg = Config::resolve(project_dir);
        cfg.knowledge_dir_abs(project_dir)
    }

    fn req(dir: &std::path::Path, op: &str, payload: Value) -> IpcRequest {
        IpcRequest {
            token: "t".into(),
            project_dir: dir.to_string_lossy().to_string(),
            op: op.into(),
            payload,
            session_id: String::new(),
        }
    }

    #[test]
    fn record_rejects_missing_begruendung() {
        let dir = tmp();
        let (resp, changed) =
            dispatch_store(&req(&dir, "record", json!({ "typ": "beobachtung", "inhalt": "x" })));
        assert!(!resp.ok && !changed);
        assert!(resp.error.unwrap().contains("begruendung"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_with_begruendung_stores_it() {
        let dir = tmp();
        let (resp, changed) = dispatch_store(&req(
            &dir,
            "record",
            json!({ "typ": "beobachtung", "inhalt": "x", "begruendung": "welche Frage?" }),
        ));
        assert!(resp.ok && changed);
        let g = ks::query(&kdir(&dir));
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].begruendung, "welche Frage?");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_fact_rejects_missing_begruendung() {
        let dir = tmp();
        let (resp, changed) = dispatch_store(&req(
            &dir,
            "add_fact",
            json!({ "frage": "Version?", "wert": "3.12", "inhalt": "x" }),
        ));
        assert!(!resp.ok && !changed);
        assert!(resp.error.unwrap().contains("begruendung"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_stamps_origin_session() {
        let dir = tmp();
        let mut r = req(&dir, "record", json!({ "typ": "beobachtung", "inhalt": "x", "begruendung": "warum?" }));
        r.session_id = "sess-42".into();
        let (resp, changed) = dispatch_store(&r);
        assert!(resp.ok && changed);
        let g = ks::query(&kdir(&dir));
        assert_eq!(g.nodes[0].session_id.as_deref(), Some("sess-42"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

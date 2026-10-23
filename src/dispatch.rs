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
        return (
            IpcResponse::err(
                "knowledge is disabled for this project (enabled=false via .claude/observer.json or OBSERVER_ENABLED)",
            ),
            false,
        );
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
                        "Filtered: {} of {} nodes. Filters: q/typ/status/tags/limit.",
                        view.nodes.len(),
                        full.nodes.len()
                    ),
                );
            }
            hints.push(
                "Compact overview (id+typ+titel). Full detail (begruendung/quellen/inhalt) per node via get_knowledge(ids:[...])."
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
                        "record: missing/invalid 'typ' (decision|insight|fact|observation|research|hypothesis)",
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
                        "record: missing 'begruendung' — give the WHY: the question it answers (observation/research/hypothesis), the rationale (insight), the purpose 'what for' (decision), or the source/where-from (fact)",
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

        // Pure read: scan + return. NOT persisted here — `mcp-serve` runs this
        // in-process (outside the daemon), and a write would make it a second writer.
        // The document index is persisted only by the daemon (`curate_scan_documents`).
        "list_documents" => {
            let docs = ks::scan_documents(&scan_dir);
            (IpcResponse::ok(json!({ "documents": docs })), false)
        }

        // ---- Human curation (Enclade's knowledge window, via `observer curate`) ----
        // Exact semantics of Enclade's former direct store writes: absent field =
        // unchanged, present field is taken verbatim (`[]` / `""` clear), ids only.
        "curate_update_node" => {
            let Some(id) = nonempty(p, "id") else {
                return (IpcResponse::err("curate_update_node: requires 'id'"), false);
            };
            let typ = match p.get("typ").and_then(Value::as_str) {
                Some(t) => match NodeType::parse(t) {
                    Some(parsed) => Some(parsed),
                    None => return (IpcResponse::err(format!("invalid typ: {t}")), false),
                },
                None => None,
            };
            let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
            let b = |k: &str| p.get(k).and_then(Value::as_bool);
            let r = ks::update_node(
                &kdir,
                id,
                s("inhalt"),
                s("begruendung"),
                typ,
                p.get("basis_score").and_then(Value::as_f64),
                raw_str_array(p, "quelle_ids"),
                raw_str_array(p, "tags"),
                b("ueberholt"),
                b("erledigt"),
            );
            store_resp(r.map(|n| json!(n)))
        }

        "curate_delete_node" => match nonempty(p, "id") {
            Some(id) => store_resp(ks::delete_node(&kdir, id).map(|_| Value::Null)),
            None => (IpcResponse::err("curate_delete_node: requires 'id'"), false),
        },

        "curate_add_edge" => {
            let (Some(von), Some(zu), Some(pol)) =
                (nonempty(p, "von"), nonempty(p, "zu"), nonempty(p, "polaritaet"))
            else {
                return (
                    IpcResponse::err("curate_add_edge: requires 'von', 'zu', 'polaritaet'"),
                    false,
                );
            };
            let gew = p.get("gewicht").and_then(Value::as_f64);
            store_resp(ks::add_edge(&kdir, von, zu, pol, gew).map(|e| json!(e)))
        }

        "curate_delete_edge" => match nonempty(p, "edge_id") {
            Some(id) => store_resp(ks::delete_edge(&kdir, id).map(|_| Value::Null)),
            None => (IpcResponse::err("curate_delete_edge: requires 'edge_id'"), false),
        },

        // Scan + persist the document index, writing only when it changed.
        // `scan_dir` (optional): relative to the project dir or absolute; default
        // `kdir.parent()` (Enclade passes ".enclade").
        "curate_scan_documents" => {
            let dir = nonempty(p, "scan_dir").map_or(scan_dir, |d| project_dir.join(d));
            let docs = ks::scan_documents(&dir);
            match ks::set_quellen(&kdir, docs.clone()) {
                Ok(changed) => (IpcResponse::ok(json!({ "documents": docs, "changed": changed })), changed),
                Err(e) => (IpcResponse::err(e.to_string()), false),
            }
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
                .filter(|n| matches!(n.typ, NodeType::Decision) && n.status == "active")
                .map(|n| json!({ "id": n.id, "inhalt": n.inhalt, "tags": n.tags }))
                .collect();
            // A question is "answered" once one value clearly leads (its fact is
            // supported) — only genuinely contested/empty ones count as open.
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
                let answered = matches!(best, Some((_, _, st)) if st == "supported");
                let entry = json!({
                    "id": fr.id, "frage": fr.inhalt, "werte": fr.werte,
                    "leading": best.map(|(w, _, _)| w),
                });
                if answered {
                    beantwortet.push(entry);
                } else {
                    offen.push(entry);
                }
            }
            (
                IpcResponse::ok(json!({
                    "active_decisions": decisions,
                    "open_questions": offen,
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
        let pol = l.get("polaritaet").and_then(Value::as_str).unwrap_or("supports");
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

/// A string-array payload field taken verbatim (None if absent; `[]` stays `Some([])`
/// so curation can clear a list). Non-string elements are ignored.
fn raw_str_array(p: &Value, key: &str) -> Option<Vec<String>> {
    let arr = p.get(key)?.as_array()?;
    Some(arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

/// Map a store result to a response; a success counts as a graph change.
fn store_resp(r: crate::error::Result<Value>) -> (IpcResponse, bool) {
    match r {
        Ok(v) => (IpcResponse::ok(v), true),
        Err(e) => (IpcResponse::err(e.to_string()), false),
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
            dispatch_store(&req(&dir, "record", json!({ "typ": "observation", "inhalt": "x" })));
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
            json!({ "typ": "observation", "inhalt": "x", "begruendung": "welche Frage?" }),
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

    /// Seed a node via the store; returns its id.
    fn seed(dir: &std::path::Path, typ: NodeType, inhalt: &str) -> String {
        ks::add_node(&kdir(dir), typ, inhalt.into(), "warum", None, "session").unwrap().id
    }

    #[test]
    fn curate_update_node_sets_all_fields_and_clears() {
        let dir = tmp();
        let id = seed(&dir, NodeType::Observation, "alt");
        let (resp, changed) = dispatch_store(&req(
            &dir,
            "curate_update_node",
            json!({ "id": id, "inhalt": "neu", "begruendung": "b", "typ": "erkenntnis",
                    "basis_score": 0.4, "quelle_ids": ["a.md"], "tags": ["t"],
                    "ueberholt": true, "erledigt": true }),
        ));
        assert!(resp.ok && changed, "{resp:?}");
        assert_eq!(resp.data["id"], json!(id));
        let n = ks::query(&kdir(&dir)).nodes.remove(0);
        assert_eq!((n.inhalt.as_str(), n.begruendung.as_str()), ("neu", "b"));
        assert_eq!(n.typ, NodeType::Insight); // German input normalized
        assert_eq!((n.basis_score, n.ueberholt, n.erledigt), (0.4, true, true));
        assert_eq!((n.quelle_ids, n.tags), (vec!["a.md".to_string()], vec!["t".to_string()]));

        // Absent = unchanged; present `[]` / `""` clear (Enclade semantics).
        let (resp, _) = dispatch_store(&req(
            &dir,
            "curate_update_node",
            json!({ "id": id, "begruendung": "", "tags": [], "quelle_ids": [] }),
        ));
        assert!(resp.ok, "{resp:?}");
        let n = ks::query(&kdir(&dir)).nodes.remove(0);
        assert_eq!(n.inhalt, "neu");
        assert!(n.begruendung.is_empty() && n.tags.is_empty() && n.quelle_ids.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn curate_update_node_rejects_bad_input() {
        let dir = tmp();
        let id = seed(&dir, NodeType::Observation, "x");
        let (resp, changed) =
            dispatch_store(&req(&dir, "curate_update_node", json!({ "id": id, "typ": "quatsch" })));
        assert!(!resp.ok && !changed);
        assert_eq!(resp.error.as_deref(), Some("invalid typ: quatsch"));
        let (resp, _) = dispatch_store(&req(&dir, "curate_update_node", json!({ "id": "nope" })));
        assert!(resp.error.unwrap().contains("node not found"));
        let (resp, _) = dispatch_store(&req(&dir, "curate_update_node", json!({})));
        assert!(!resp.ok);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn curate_delete_node_removes_node_and_its_edges() {
        let dir = tmp();
        let a = seed(&dir, NodeType::Insight, "a");
        let b = seed(&dir, NodeType::Observation, "b");
        ks::add_edge(&kdir(&dir), &a, &b, "supports", None).unwrap();
        let (resp, changed) = dispatch_store(&req(&dir, "curate_delete_node", json!({ "id": b })));
        assert!(resp.ok && changed && resp.data.is_null(), "{resp:?}");
        let g = ks::query(&kdir(&dir));
        assert_eq!(g.nodes.len(), 1);
        assert!(g.edges.is_empty());
        assert!(!kdir(&dir).join("nodes").join(format!("{b}.md")).exists());
        let (resp, _) = dispatch_store(&req(&dir, "curate_delete_node", json!({ "id": b })));
        assert!(resp.error.unwrap().contains("node not found"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn curate_add_and_delete_edge() {
        let dir = tmp();
        let a = seed(&dir, NodeType::Insight, "alpha");
        let b = seed(&dir, NodeType::Observation, "beta");
        let (resp, changed) = dispatch_store(&req(
            &dir,
            "curate_add_edge",
            json!({ "von": a, "zu": b, "polaritaet": "widerspricht", "gewicht": 0.5 }),
        ));
        assert!(resp.ok && changed, "{resp:?}");
        assert_eq!(resp.data["polaritaet"], json!("contradicts"));
        assert_eq!(resp.data["gewicht"], json!(0.5));
        let edge_id = resp.data["id"].as_str().unwrap().to_string();

        // Ids only — a title fragment must NOT resolve (unlike `link`).
        let (resp, _) = dispatch_store(&req(
            &dir,
            "curate_add_edge",
            json!({ "von": a, "zu": "beta", "polaritaet": "supports" }),
        ));
        assert!(resp.error.unwrap().contains("node not found"));
        // Cycle is rejected.
        let (resp, _) =
            dispatch_store(&req(&dir, "curate_add_edge", json!({ "von": b, "zu": a, "polaritaet": "+" })));
        assert!(resp.error.unwrap().contains("cycle"));

        let (resp, changed) =
            dispatch_store(&req(&dir, "curate_delete_edge", json!({ "edge_id": edge_id })));
        assert!(resp.ok && changed);
        assert!(ks::query(&kdir(&dir)).edges.is_empty());
        let (resp, _) = dispatch_store(&req(&dir, "curate_delete_edge", json!({ "edge_id": edge_id })));
        assert!(resp.error.unwrap().contains("edge not found"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn curate_scan_documents_persists_only_on_change() {
        let dir = tmp();
        let docs = dir.join(".enclade").join("features");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("f.md"), "# Feature F").unwrap();
        let scan = |d: &std::path::Path| {
            dispatch_store(&req(d, "curate_scan_documents", json!({ "scan_dir": ".enclade" })))
        };
        let (resp, changed) = scan(&dir);
        assert!(resp.ok && changed, "{resp:?}");
        assert_eq!(resp.data["changed"], json!(true));
        assert_eq!(resp.data["documents"][0]["id"], json!(".enclade/features/f.md"));
        assert_eq!(ks::query(&kdir(&dir)).quellen.len(), 1);

        let (resp, changed) = scan(&dir);
        assert!(resp.ok && !changed);
        assert_eq!(resp.data["changed"], json!(false));

        std::fs::write(docs.join("g.md"), "# G").unwrap();
        let (resp, changed) = scan(&dir);
        assert!(changed && resp.data["documents"].as_array().unwrap().len() == 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_documents_does_not_persist() {
        let dir = tmp();
        let docs = kdir(&dir).parent().unwrap().join("adr");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("1.md"), "# ADR 1").unwrap();
        let (resp, changed) = dispatch_store(&req(&dir, "list_documents", json!({})));
        assert!(resp.ok && !changed);
        assert_eq!(resp.data["documents"].as_array().unwrap().len(), 1);
        assert!(!kdir(&dir).join("quellen.json").exists(), "read path must not write");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_stamps_origin_session() {
        let dir = tmp();
        let mut r = req(&dir, "record", json!({ "typ": "observation", "inhalt": "x", "begruendung": "warum?" }));
        r.session_id = "sess-42".into();
        let (resp, changed) = dispatch_store(&r);
        assert!(resp.ok && changed);
        let g = ks::query(&kdir(&dir));
        assert_eq!(g.nodes[0].session_id.as_deref(), Some("sess-42"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

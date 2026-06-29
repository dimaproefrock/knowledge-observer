//! Persistence + ops for a project's knowledge graph (the `knowledge/` dir).
//!
//! Includes the real bottom-up scoring engine (`recompute_scores`, concept §4),
//! cycle-safe edge ops, hygiene hints, manual curation, and read-side filtering.
//!
//! The GUI is the single writer (MCP server is a thin client over IPC), so a
//! process-global lock serializes the load→mutate→save sequence.
//!
//! ## On-disk layout
//! The store persists as **one Markdown file per node** plus sidecar JSON files,
//! all directly under the passed `knowledge_dir`:
//! - `nodes/<id>.md` — YAML-style `---` frontmatter (every scalar/array field of
//!   `Node`) + a human-readable body (`inhalt`, then a sentinel separator line,
//!   then `begruendung`). Round-trips every field exactly.
//! - `edges.json`  — the `Vec<Edge>`.
//! - `fragen.json` — the `Vec<Frage>`.
//! - `quellen.json` — the `Vec<Quelle>`.
//!
//! Writes are atomic (temp file + rename) so lock-free readers (index hook, GUI)
//! never observe a torn file. A one-time, idempotent migration converts a legacy
//! `graph.json` into this layout — see `migrate`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::migrate;
use crate::store::now;

/// Serializes read-modify-write on the graph file. The GUI is the only writer,
/// but its IPC handler runs one worker thread per connection.
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Node type. Variant names + serialized codes are English (public release). The
/// schema field KEYS (`typ`, `status`, `inhalt`, …) stay as-is on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    Decision,
    Insight,
    Fact,
    Observation,
    Research,
    Hypothesis,
}

impl NodeType {
    /// Concept §3.2 type defaults (overridable per node).
    pub fn default_basis_score(self) -> f64 {
        match self {
            NodeType::Fact => 0.95,
            NodeType::Observation => 0.90,
            NodeType::Research => 0.70,
            NodeType::Hypothesis => 0.30,
            // Inner nodes: score is aggregated from children by the real engine.
            NodeType::Insight | NodeType::Decision => 0.50,
        }
    }

    /// Parse a node-type code. Accepts BOTH the new English codes AND the legacy
    /// German codes (back-compat: existing stores hold German `typ:` values). The
    /// engine always SERIALIZES the English form, so a load/save flips a store to
    /// English. Keep the German arms until all known stores are migrated.
    pub fn parse(s: &str) -> Option<NodeType> {
        match s.trim().to_lowercase().as_str() {
            // English (canonical)
            "decision" => Some(NodeType::Decision),
            "insight" => Some(NodeType::Insight),
            "fact" => Some(NodeType::Fact),
            "observation" => Some(NodeType::Observation),
            "research" => Some(NodeType::Research),
            "hypothesis" => Some(NodeType::Hypothesis),
            // Legacy German (back-compat read)
            "entscheidung" => Some(NodeType::Decision),
            "erkenntnis" => Some(NodeType::Insight),
            "fakt" => Some(NodeType::Fact),
            "beobachtung" => Some(NodeType::Observation),
            "recherche" => Some(NodeType::Research),
            "vermutung" => Some(NodeType::Hypothesis),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub typ: NodeType,
    pub inhalt: String,
    /// Type-adaptive rationale (required at the API): why this node exists —
    /// the question it answers (evidence), the reasoning behind it (insight),
    /// its purpose (decision = "what for"), or its source (fact = "where from").
    /// `#[serde(default)]` so pre-field graphs still load (empty = unknown).
    #[serde(default)]
    pub begruendung: String,
    /// RFC3339. Metadata only — never feeds the score (concept §3.2, §5).
    pub datum: String,
    pub basis_score: f64,
    /// Computed by the bottom-up engine (`recompute_scores`, §4). Leaves = basis.
    pub score: f64,
    /// Computed: unsupported | supported | disputed | refuted (lifecycle states
    /// active | superseded | done override the belief status — see the engine).
    pub status: String,
    /// "session" (recorded by Claude during work) | "manual" (human-curated).
    pub herkunft: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frage_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wert: Option<String>,
    /// Provenance: ids (= relative paths) of documents this node cites. Documents
    /// are NOT graph nodes and never affect the score — pure source links.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quelle_ids: Vec<String>,
    /// Origin: the session that recorded this node (provenance only — does NOT
    /// scope visibility or affect the score). None for human/manual or legacy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Free-form topic/area tags (e.g. "pathfinding", "voxel") for targeted
    /// retrieval across work-streams. Durable; does not affect the score.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Lifecycle: explicitly retired ("superseded") without a replacement. A node is
    /// also effectively retired when a `replaces` edge points at it. Decisions get
    /// status active/superseded from this instead of the belief status.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ueberholt: bool,
    /// Lifecycle: carried out / done but NOT replaced. Drops out of "what holds
    /// now" without being wrong. Precedence: superseded > done > active.
    #[serde(default, skip_serializing_if = "is_false")]
    pub erledigt: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    /// Parent (the dependent node).
    pub von: String,
    /// Child (the supporting/contradicting node).
    pub zu: String,
    /// "supports" | "contradicts" | "replaces".
    pub polaritaet: String,
    pub gewicht: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frage {
    pub id: String,
    pub inhalt: String,
    #[serde(default)]
    pub werte: Vec<String>,
}

/// A document indexed as a citable source (auto-scanned). Lives in a side-list,
/// NOT in the typed/scored DAG — keeps the concept clean (a document is where
/// knowledge comes from, not knowledge itself).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quelle {
    /// Stable id = the relative path (so re-scans dedupe and citations are stable).
    pub id: String,
    /// Relative to the project root, e.g. ".claude/adr/003-foo.md".
    pub pfad: String,
    pub titel: String,
    /// Derived from the containing subfolder: adr|spec|feature|note|doc|<folder>.
    pub art: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnowledgeGraph {
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub fragen: Vec<Frage>,
    #[serde(default)]
    pub quellen: Vec<Quelle>,
}

fn nodes_dir(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("nodes")
}

fn edges_path(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("edges.json")
}

fn fragen_path(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("fragen.json")
}

fn quellen_path(knowledge_dir: &Path) -> PathBuf {
    knowledge_dir.join("quellen.json")
}

/// Full-line sentinel separating `inhalt` (above) from `begruendung` (below) in a
/// node `.md` body. Split happens on the FIRST occurrence, so `inhalt` may not
/// contain this exact line on its own — extremely unlikely for prose, and the
/// round-trip test pins multiline content.
const BODY_SEP: &str = "<!-- begründung -->";

/// Load the graph from the `.md`/JSON layout, migrating a legacy `graph.json`
/// first if present. Missing/corrupt files yield empty collections (never panics).
pub fn load(knowledge_dir: &Path) -> KnowledgeGraph {
    migrate::migrate_if_needed(knowledge_dir);
    load_layout(knowledge_dir)
}

/// Read the on-disk layout (nodes/*.md + sidecar JSON) without migrating. Used by
/// `load` after migration and by tests.
pub(crate) fn load_layout(knowledge_dir: &Path) -> KnowledgeGraph {
    let mut nodes: Vec<Node> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(nodes_dir(knowledge_dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Some(node) = parse_node_md(&content) {
                    nodes.push(node);
                }
            }
        }
    }
    // Deterministic order (by datum then id) so callers/tests see a stable layout.
    nodes.sort_by(|a, b| a.datum.cmp(&b.datum).then_with(|| a.id.cmp(&b.id)));

    KnowledgeGraph {
        nodes,
        edges: read_json_vec(&edges_path(knowledge_dir)),
        fragen: read_json_vec(&fragen_path(knowledge_dir)),
        quellen: read_json_vec(&quellen_path(knowledge_dir)),
    }
}

fn read_json_vec<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Persist the whole graph in the `.md`/JSON layout. Each node is written
/// atomically (temp + rename); nodes whose `.md` no longer corresponds to a
/// current node are removed (handles deletes/merges). Sidecar JSON files
/// (edges/fragen/quellen) are rewritten atomically too.
fn save(knowledge_dir: &Path, graph: &KnowledgeGraph) -> Result<()> {
    let ndir = nodes_dir(knowledge_dir);
    std::fs::create_dir_all(&ndir)?;

    // Write/refresh every current node's .md.
    let mut wanted: HashSet<String> = HashSet::new();
    for node in &graph.nodes {
        wanted.insert(format!("{}.md", node.id));
        let path = ndir.join(format!("{}.md", node.id));
        atomic_write(&path, serialize_node_md(node).as_bytes())?;
    }

    // Remove stale node files (deleted/merged-away nodes).
    if let Ok(entries) = std::fs::read_dir(&ndir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !wanted.contains(name) {
                std::fs::remove_file(&path).ok();
            }
        }
    }

    // Sidecar lists (atomic).
    atomic_write(&edges_path(knowledge_dir), serde_json::to_string_pretty(&graph.edges)?.as_bytes())?;
    atomic_write(&fragen_path(knowledge_dir), serde_json::to_string_pretty(&graph.fragen)?.as_bytes())?;
    atomic_write(&quellen_path(knowledge_dir), serde_json::to_string_pretty(&graph.quellen)?.as_bytes())?;
    // Keep the directory present even for an empty graph.
    std::fs::create_dir_all(knowledge_dir)?;
    Ok(())
}

/// Write `bytes` to `path` atomically: a uniquely-named temp file in the same
/// directory, then `rename` into place. Concurrent readers see either the old or
/// the new file in full — never a partial write.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("knowledge"),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&tmp, bytes)?;
    // rename is atomic on the same filesystem on both Windows and Unix when the
    // target is replaced; std::fs::rename replaces an existing file on both.
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            std::fs::remove_file(&tmp).ok();
            Err(e.into())
        }
    }
}

// ===================== Node <-> Markdown (frontmatter) =====================

/// Serialize one `Node` to its `.md` representation: `---` YAML-ish frontmatter
/// (each value JSON-encoded so any string round-trips exactly, arrays as JSON
/// arrays) + a human-readable body (`inhalt`, a sentinel line, then `begruendung`).
pub(crate) fn serialize_node_md(node: &Node) -> String {
    let mut fm = String::new();
    fm.push_str("---\n");
    let s = |v: &str| serde_json::to_string(v).unwrap_or_else(|_| "\"\"".into());
    fm.push_str(&format!("id: {}\n", s(&node.id)));
    fm.push_str(&format!("typ: {}\n", s(node_type_str(node.typ))));
    fm.push_str(&format!("datum: {}\n", s(&node.datum)));
    fm.push_str(&format!("basis_score: {}\n", node.basis_score));
    fm.push_str(&format!("score: {}\n", node.score));
    fm.push_str(&format!("status: {}\n", s(&node.status)));
    fm.push_str(&format!("herkunft: {}\n", s(&node.herkunft)));
    if let Some(fid) = &node.frage_id {
        fm.push_str(&format!("frage_id: {}\n", s(fid)));
    }
    if let Some(wert) = &node.wert {
        fm.push_str(&format!("wert: {}\n", s(wert)));
    }
    if let Some(sid) = &node.session_id {
        fm.push_str(&format!("session_id: {}\n", s(sid)));
    }
    fm.push_str(&format!(
        "quelle_ids: {}\n",
        serde_json::to_string(&node.quelle_ids).unwrap_or_else(|_| "[]".into())
    ));
    fm.push_str(&format!(
        "tags: {}\n",
        serde_json::to_string(&node.tags).unwrap_or_else(|_| "[]".into())
    ));
    fm.push_str(&format!("ueberholt: {}\n", node.ueberholt));
    fm.push_str(&format!("erledigt: {}\n", node.erledigt));
    fm.push_str("---\n\n");

    fm.push_str(&node.inhalt);
    fm.push_str("\n\n");
    fm.push_str(BODY_SEP);
    fm.push('\n');
    fm.push_str(&node.begruendung);
    fm.push('\n');
    fm
}

/// Parse a node `.md` back into a `Node`. Returns `None` if the frontmatter is
/// missing/malformed (caller skips it, keeping `load` infallible).
pub(crate) fn parse_node_md(content: &str) -> Option<Node> {
    let rest = content.strip_prefix("---\n").or_else(|| content.strip_prefix("---\r\n"))?;
    // Frontmatter ends at the first line that is exactly "---".
    let mut fm_lines: Vec<&str> = Vec::new();
    let mut body_start = 0usize;
    let mut found_end = false;
    let mut consumed = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        consumed += line.len();
        if trimmed == "---" {
            found_end = true;
            body_start = consumed;
            break;
        }
        fm_lines.push(trimmed);
    }
    if !found_end {
        return None;
    }
    // Strip the single blank line we write between frontmatter and body
    // (`---\n\n`). Robust to CRLF.
    let body = &rest[body_start..];
    let body = body.strip_prefix("\r\n").or_else(|| body.strip_prefix('\n')).unwrap_or(body);

    let mut map: HashMap<String, String> = HashMap::new();
    for line in &fm_lines {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(idx) = line.find(':') {
            let key = line[..idx].trim().to_string();
            let val = line[idx + 1..].trim().to_string();
            map.insert(key, val);
        }
    }

    let get_str = |k: &str| -> Option<String> {
        map.get(k).map(|v| serde_json::from_str::<String>(v).unwrap_or_else(|_| v.clone()))
    };
    let get_vec = |k: &str| -> Vec<String> {
        map.get(k).and_then(|v| serde_json::from_str::<Vec<String>>(v).ok()).unwrap_or_default()
    };
    let get_bool = |k: &str| -> bool { map.get(k).map(|v| v.trim() == "true").unwrap_or(false) };
    let get_f64 = |k: &str| -> f64 { map.get(k).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0) };

    let id = get_str("id")?;
    let typ = NodeType::parse(&get_str("typ").unwrap_or_default())?;

    // Body: inhalt up to the first BODY_SEP line, begruendung after it.
    let (inhalt, begruendung) = split_body(body);

    Some(Node {
        id,
        typ,
        inhalt,
        begruendung,
        datum: get_str("datum").unwrap_or_default(),
        basis_score: get_f64("basis_score"),
        score: get_f64("score"),
        status: get_str("status").unwrap_or_default(),
        herkunft: get_str("herkunft").unwrap_or_default(),
        frage_id: get_str("frage_id"),
        wert: get_str("wert"),
        quelle_ids: get_vec("quelle_ids"),
        session_id: get_str("session_id"),
        tags: get_vec("tags"),
        ueberholt: get_bool("ueberholt"),
        erledigt: get_bool("erledigt"),
    })
}

/// Split a node body into (`inhalt`, `begruendung`) on the first full `BODY_SEP`
/// line. The exactly-one blank line padding written by `serialize_node_md` is
/// stripped so content round-trips byte-for-byte.
fn split_body(body: &str) -> (String, String) {
    // Find the first line equal to BODY_SEP.
    let mut offset = 0usize;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == BODY_SEP {
            let inhalt = body[..offset].to_string();
            let begruendung = body[offset + line.len()..].to_string();
            return (trim_pad(&inhalt), trim_one_trailing_newline(&begruendung));
        }
        offset += line.len();
    }
    // No separator → whole body is inhalt (defensive; legacy/hand-edited).
    (trim_pad(body), String::new())
}

/// Strip the `\n\n` we append after `inhalt` before the separator.
fn trim_pad(s: &str) -> String {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.to_string()
}

/// Strip the single trailing `\n` we append after `begruendung`.
fn trim_one_trailing_newline(s: &str) -> String {
    s.strip_suffix('\n').map(|t| t.to_string()).unwrap_or_else(|| s.to_string())
}

/// Lowercase tag for a `NodeType` (matches `#[serde(rename_all = "lowercase")]`).
fn node_type_str(t: NodeType) -> &'static str {
    match t {
        NodeType::Decision => "decision",
        NodeType::Insight => "insight",
        NodeType::Fact => "fact",
        NodeType::Observation => "observation",
        NodeType::Research => "research",
        NodeType::Hypothesis => "hypothesis",
    }
}

/// Global skepticism: unsupported knowledge is not automatically certain.
const W: f64 = 1.0;
const EPS: f64 = 1e-9;

/// Bottom-up scoring engine (concept §4). Pure + deterministic, a single wave
/// over the DAG (no fixpoint): children (`zu`) are computed before parents
/// (`von`). Decisions taken (offene Punkte 1–4):
/// - **Leaves** (Observation/Research/Hypothesis): `score = basis_score`; status
///   "supported" unless contradicted from below (rare).
/// - **Fact**: `score = masse(wert)/(Σmasse+W)` within its Frage; status from the
///   competing-value mass share (Punkt 2).
/// - **Insight/Decision**: `pro/con = Σ score(kind)·gewicht`;
///   `score = pro/(pro+con+W)`; status from `r = con/(pro+con)` (Punkt 3:
///   score = Stärke, status = Einigkeit). Multi-path evidence reinforces (Punkt 1).
fn recompute_scores(graph: &mut KnowledgeGraph) {
    // --- Fakt masses per Frage (value mass = Σ basis_score of facts on it) ---
    let mut masse: HashMap<(String, String), f64> = HashMap::new();
    let mut total: HashMap<String, f64> = HashMap::new();
    for n in &graph.nodes {
        if n.typ == NodeType::Fact {
            if let (Some(fid), Some(wert)) = (&n.frage_id, &n.wert) {
                *masse.entry((fid.clone(), wert.clone())).or_insert(0.0) += n.basis_score;
                *total.entry(fid.clone()).or_insert(0.0) += n.basis_score;
            }
        }
    }

    // --- Topological order: `zu` before `von` (a parent depends on its child) ---
    let ids: Vec<String> = graph.nodes.iter().map(|n| n.id.clone()).collect();
    let id_set: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
    let mut in_deg: HashMap<String, usize> = ids.iter().map(|i| (i.clone(), 0usize)).collect();
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new(); // zu -> [von, ...]
    for e in &graph.edges {
        if id_set.contains(e.von.as_str()) && id_set.contains(e.zu.as_str()) {
            *in_deg.get_mut(&e.von).unwrap() += 1;
            dependents.entry(e.zu.clone()).or_default().push(e.von.clone());
        }
    }
    let mut queue: Vec<String> =
        in_deg.iter().filter(|(_, d)| **d == 0).map(|(i, _)| i.clone()).collect();
    let mut order: Vec<String> = Vec::with_capacity(ids.len());
    while let Some(n) = queue.pop() {
        order.push(n.clone());
        if let Some(deps) = dependents.get(&n).cloned() {
            for von in deps {
                let d = in_deg.get_mut(&von).unwrap();
                *d -= 1;
                if *d == 0 {
                    queue.push(von);
                }
            }
        }
    }
    if order.len() < ids.len() {
        // Defensive: add_edge prevents cycles, but never drop nodes if one slips in.
        for i in &ids {
            if !order.contains(i) {
                order.push(i.clone());
            }
        }
    }

    // Snapshot the fields the wave needs (avoids borrow conflicts with write-back).
    struct Info {
        typ: NodeType,
        basis: f64,
        frage_id: Option<String>,
        wert: Option<String>,
    }
    let info: HashMap<String, Info> = graph
        .nodes
        .iter()
        .map(|n| {
            (
                n.id.clone(),
                Info { typ: n.typ, basis: n.basis_score, frage_id: n.frage_id.clone(), wert: n.wert.clone() },
            )
        })
        .collect();
    let mut children: HashMap<String, Vec<(String, String, f64)>> = HashMap::new();
    // `replaces` edges are lifecycle links, NOT evidence: the `zu` (old) node is
    // superseded; they never feed pro/con. (Legacy German "ersetzt" still honored.)
    let mut superseded: HashSet<String> = HashSet::new();
    for e in &graph.edges {
        if e.polaritaet == "replaces" || e.polaritaet == "ersetzt" {
            superseded.insert(e.zu.clone());
            continue;
        }
        children.entry(e.von.clone()).or_default().push((e.zu.clone(), e.polaritaet.clone(), e.gewicht));
    }

    // --- The wave ---
    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut statuses: HashMap<String, String> = HashMap::new();
    for id in &order {
        let inf = &info[id];
        let mut pro = 0.0;
        let mut con = 0.0;
        if let Some(cs) = children.get(id) {
            for (zu, pol, gew) in cs {
                let child = *scores.get(zu).unwrap_or(&0.0);
                // "contradicts" (legacy German "widerspricht") = con; else pro.
                if pol == "contradicts" || pol == "widerspricht" {
                    con += child * gew;
                } else {
                    pro += child * gew;
                }
            }
        }
        let gesamt = pro + con;
        let (score, status) = match inf.typ {
            NodeType::Observation | NodeType::Research | NodeType::Hypothesis => {
                let st = if gesamt <= EPS { "supported".to_string() } else { status_from(pro, con) };
                (inf.basis, st)
            }
            NodeType::Fact => match (&inf.frage_id, &inf.wert) {
                (Some(fid), Some(wert)) => {
                    let t = *total.get(fid).unwrap_or(&0.0);
                    let m = *masse.get(&(fid.clone(), wert.clone())).unwrap_or(&0.0);
                    let sc = m / (t + W);
                    (sc, fakt_status(m, t))
                }
                _ => (inf.basis, "supported".to_string()),
            },
            NodeType::Insight | NodeType::Decision => {
                (pro / (pro + con + W), status_from(pro, con))
            }
        };
        scores.insert(id.clone(), score);
        statuses.insert(id.clone(), status);
    }

    // --- Write back ---
    // Lifecycle overrides the belief status: a retired node (explicit flag or
    // superseded by a `replaces` edge) is "superseded"; a live `decision` is
    // "active" (a choice isn't "weak" just for lacking evidence — its score still
    // reflects how grounded it is). All other types keep their belief status.
    for n in &mut graph.nodes {
        if let Some(s) = scores.get(&n.id) {
            n.score = *s;
        }
        let retired = n.ueberholt || superseded.contains(&n.id);
        n.status = if retired {
            "superseded".to_string()
        } else if n.erledigt {
            "done".to_string()
        } else if n.typ == NodeType::Decision {
            "active".to_string()
        } else {
            statuses.get(&n.id).cloned().unwrap_or_default()
        };
    }
}

/// Status from a pro/con pair (concept §4.4): `r = con/(pro+con)` — the conflict
/// share, independent of `W`. Little evidence → "unsupported".
fn status_from(pro: f64, con: f64) -> String {
    let gesamt = pro + con;
    if gesamt <= EPS {
        return "unsupported".to_string();
    }
    let r = con / gesamt;
    if r < 0.2 {
        "supported".to_string()
    } else if r > 0.8 {
        "refuted".to_string()
    } else {
        "disputed".to_string()
    }
}

/// Status of a Fakt from its Frage (Punkt 2): own value mass = "pro", competing
/// values' mass = "con".
fn fakt_status(own_masse: f64, total_masse: f64) -> String {
    if total_masse <= EPS {
        return "unsupported".to_string();
    }
    status_from(own_masse, total_masse - own_masse)
}

/// Would adding edge `von → zu` create a cycle? True if `zu` can already reach
/// `von` along existing `von → zu` edges (or it's a self-loop). The graph must
/// stay a DAG (concept §3.3).
fn would_cycle(edges: &[Edge], von: &str, zu: &str) -> bool {
    if von == zu {
        return true;
    }
    let mut stack = vec![zu.to_string()];
    let mut seen = HashSet::new();
    while let Some(cur) = stack.pop() {
        if cur == von {
            return true;
        }
        if !seen.insert(cur.clone()) {
            continue;
        }
        for e in edges {
            if e.von == cur {
                stack.push(e.zu.clone());
            }
        }
    }
    false
}

/// Read the whole graph (for `query_knowledge` / the window). Scores/statuses are
/// recomputed on read (deterministic, no save) so callers always see current
/// values — this also rescores legacy graphs written before the engine existed.
pub fn query(knowledge_dir: &Path) -> KnowledgeGraph {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    recompute_scores(&mut graph);
    graph
}

/// Read-side filter for `query_knowledge` (the window always reads the full graph).
#[derive(Debug, Default, Clone)]
pub struct QueryFilter {
    /// Keyword in `inhalt` (case-insensitive substring).
    pub q: Option<String>,
    /// Restrict to one node type.
    pub typ: Option<NodeType>,
    /// Restrict to one status (supported|disputed|refuted|unsupported).
    pub status: Option<String>,
    /// Keep only the top-N core matches by score.
    pub limit: Option<usize>,
    /// Match nodes carrying ANY of these tags (case-insensitive).
    pub tags: Vec<String>,
}

impl QueryFilter {
    pub fn is_active(&self) -> bool {
        self.q.is_some()
            || self.typ.is_some()
            || self.status.is_some()
            || self.limit.is_some()
            || !self.tags.is_empty()
    }
}

/// Subset view of an **already-scored** graph: core matches (keyword/type/status),
/// capped to the top-N by score, plus their 1-hop neighbours so matches keep their
/// evidence/context. Edges survive only when both endpoints do; fragen when still
/// referenced. Pure — no I/O, no scoring (call after `recompute_scores`).
pub fn apply_filter(graph: &KnowledgeGraph, f: &QueryFilter) -> KnowledgeGraph {
    if !f.is_active() {
        return graph.clone();
    }
    let q_lc = f.q.as_ref().map(|s| s.to_lowercase());
    let mut core: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| {
            if let Some(q) = &q_lc {
                if !n.inhalt.to_lowercase().contains(q.as_str()) {
                    return false;
                }
            }
            if let Some(t) = f.typ {
                if n.typ != t {
                    return false;
                }
            }
            if let Some(st) = &f.status {
                if &n.status != st {
                    return false;
                }
            }
            if !f.tags.is_empty() {
                let want: Vec<String> = f.tags.iter().map(|t| t.to_lowercase()).collect();
                let has = n.tags.iter().any(|nt| want.contains(&nt.to_lowercase()));
                if !has {
                    return false;
                }
            }
            true
        })
        .collect();
    core.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(lim) = f.limit {
        core.truncate(lim);
    }
    let core_ids: HashSet<String> = core.iter().map(|n| n.id.clone()).collect();
    let mut keep = core_ids.clone();
    for e in &graph.edges {
        if core_ids.contains(&e.von) {
            keep.insert(e.zu.clone());
        }
        if core_ids.contains(&e.zu) {
            keep.insert(e.von.clone());
        }
    }
    let nodes: Vec<Node> = graph.nodes.iter().filter(|n| keep.contains(&n.id)).cloned().collect();
    let edges: Vec<Edge> = graph
        .edges
        .iter()
        .filter(|e| keep.contains(&e.von) && keep.contains(&e.zu))
        .cloned()
        .collect();
    let frage_ids: HashSet<&str> = nodes.iter().filter_map(|n| n.frage_id.as_deref()).collect();
    let fragen: Vec<Frage> =
        graph.fragen.iter().filter(|fr| frage_ids.contains(fr.id.as_str())).cloned().collect();
    // Keep the full document index in the view (it's the source list, not graph-size-bound).
    KnowledgeGraph { nodes, edges, fragen, quellen: graph.quellen.clone() }
}

/// Deep fetch by id: the given seed nodes plus their neighbourhood up to `depth`
/// hops (edges both directions), in FULL detail. The "address known knowledge"
/// path — the agent picks ids from the compact map, then pulls the relevant
/// sub-DAG. Pure; call after `recompute_scores`.
pub fn get_subgraph(graph: &KnowledgeGraph, ids: &[String], depth: usize) -> KnowledgeGraph {
    let mut keep: HashSet<String> = ids.iter().cloned().collect();
    for _ in 0..depth {
        let mut added = false;
        for e in &graph.edges {
            if keep.contains(&e.von) && !keep.contains(&e.zu) {
                keep.insert(e.zu.clone());
                added = true;
            }
            if keep.contains(&e.zu) && !keep.contains(&e.von) {
                keep.insert(e.von.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    let nodes: Vec<Node> = graph.nodes.iter().filter(|n| keep.contains(&n.id)).cloned().collect();
    let edges: Vec<Edge> = graph
        .edges
        .iter()
        .filter(|e| keep.contains(&e.von) && keep.contains(&e.zu))
        .cloned()
        .collect();
    let frage_ids: HashSet<&str> = nodes.iter().filter_map(|n| n.frage_id.as_deref()).collect();
    let fragen: Vec<Frage> =
        graph.fragen.iter().filter(|fr| frage_ids.contains(fr.id.as_str())).cloned().collect();
    KnowledgeGraph { nodes, edges, fragen, quellen: graph.quellen.clone() }
}

/// Add a standalone node (leaf or inner). `basis_score` falls back to the type
/// default. Returns the stored node (with computed score/status).
pub fn add_node(
    knowledge_dir: &Path,
    typ: NodeType,
    inhalt: String,
    begruendung: &str,
    basis_score: Option<f64>,
    herkunft: &str,
) -> Result<Node> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    let node = Node {
        id: uuid::Uuid::new_v4().to_string(),
        typ,
        inhalt,
        begruendung: begruendung.to_string(),
        datum: now(),
        basis_score: basis_score.unwrap_or_else(|| typ.default_basis_score()),
        score: 0.0,
        status: String::new(),
        herkunft: herkunft.to_string(),
        frage_id: None,
        wert: None,
        quelle_ids: vec![],
        session_id: None,
        tags: vec![],
        ueberholt: false,
        erledigt: false,
    };
    let id = node.id.clone();
    graph.nodes.push(node);
    recompute_scores(&mut graph);
    save(knowledge_dir, &graph)?;
    Ok(graph.nodes.into_iter().find(|n| n.id == id).expect("just added"))
}

/// Add a polarized edge `von → zu`. Errors if either node is missing or the edge
/// would create a cycle.
pub fn add_edge(
    knowledge_dir: &Path,
    von: &str,
    zu: &str,
    polaritaet: &str,
    gewicht: Option<f64>,
) -> Result<Edge> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    if !graph.nodes.iter().any(|n| n.id == von) {
        return Err(Error::msg(format!("node not found: {von}")));
    }
    if !graph.nodes.iter().any(|n| n.id == zu) {
        return Err(Error::msg(format!("node not found: {zu}")));
    }
    if would_cycle(&graph.edges, von, zu) {
        return Err(Error::msg(
            "edge would create a cycle (graph must stay a DAG)".to_string(),
        ));
    }
    let pol = normalize_polaritaet(polaritaet)?;
    let edge = Edge {
        id: uuid::Uuid::new_v4().to_string(),
        von: von.to_string(),
        zu: zu.to_string(),
        polaritaet: pol,
        gewicht: gewicht.unwrap_or(1.0),
    };
    graph.edges.push(edge.clone());
    recompute_scores(&mut graph);
    save(knowledge_dir, &graph)?;
    Ok(edge)
}

/// Normalize an edge-polarity input to the canonical English code. Accepts the new
/// English codes, the legacy German codes (back-compat), and common shorthands.
fn normalize_polaritaet(p: &str) -> Result<String> {
    match p.trim().to_lowercase().as_str() {
        "supports" | "stuetzt" | "stützt" | "+" | "pro" => Ok("supports".to_string()),
        "contradicts" | "widerspricht" | "-" | "contra" | "con" => Ok("contradicts".to_string()),
        // Lifecycle link: `von` (the new decision) replaces `zu` (the old one).
        // Excluded from the belief score; marks `zu` as superseded.
        "replaces" | "supersedes" | "ersetzt" | "überholt" | "ueberholt" => Ok("replaces".to_string()),
        other => Err(Error::msg(format!(
            "invalid polaritaet '{other}' (use 'supports', 'contradicts' or 'replaces')"
        ))),
    }
}

/// Add a Fakt for a value under a question (creating the question if needed).
pub fn add_fact(
    knowledge_dir: &Path,
    frage_inhalt: &str,
    wert: &str,
    inhalt: String,
    begruendung: &str,
    basis_score: Option<f64>,
    herkunft: &str,
) -> Result<Node> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);

    // Find-or-create the question (match on inhalt).
    let frage_id = match graph.fragen.iter().find(|f| f.inhalt == frage_inhalt) {
        Some(f) => f.id.clone(),
        None => {
            let f = Frage {
                id: uuid::Uuid::new_v4().to_string(),
                inhalt: frage_inhalt.to_string(),
                werte: vec![],
            };
            let id = f.id.clone();
            graph.fragen.push(f);
            id
        }
    };
    // Register the value on the question.
    if let Some(f) = graph.fragen.iter_mut().find(|f| f.id == frage_id) {
        if !f.werte.iter().any(|w| w == wert) {
            f.werte.push(wert.to_string());
        }
    }

    let node = Node {
        id: uuid::Uuid::new_v4().to_string(),
        typ: NodeType::Fact,
        inhalt,
        begruendung: begruendung.to_string(),
        datum: now(),
        basis_score: basis_score.unwrap_or_else(|| NodeType::Fact.default_basis_score()),
        score: 0.0,
        status: String::new(),
        herkunft: herkunft.to_string(),
        frage_id: Some(frage_id),
        wert: Some(wert.to_string()),
        quelle_ids: vec![],
        session_id: None,
        tags: vec![],
        ueberholt: false,
        erledigt: false,
    };
    let id = node.id.clone();
    graph.nodes.push(node);
    recompute_scores(&mut graph);
    save(knowledge_dir, &graph)?;
    Ok(graph.nodes.into_iter().find(|n| n.id == id).expect("just added"))
}

/// Update a node's editable fields (human curation). `None` keeps the current
/// value. Returns the updated node.
#[allow(clippy::too_many_arguments)]
pub fn update_node(
    knowledge_dir: &Path,
    node_id: &str,
    inhalt: Option<String>,
    begruendung: Option<String>,
    typ: Option<NodeType>,
    basis_score: Option<f64>,
    quelle_ids: Option<Vec<String>>,
    tags: Option<Vec<String>>,
    ueberholt: Option<bool>,
    erledigt: Option<bool>,
) -> Result<Node> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    {
        let node = graph
            .nodes
            .iter_mut()
            .find(|n| n.id == node_id)
            .ok_or_else(|| Error::msg(format!("node not found: {node_id}")))?;
        if let Some(i) = inhalt {
            node.inhalt = i;
        }
        if let Some(b) = begruendung {
            node.begruendung = b;
        }
        if let Some(t) = typ {
            node.typ = t;
        }
        if let Some(b) = basis_score {
            node.basis_score = b;
        }
        if let Some(q) = quelle_ids {
            node.quelle_ids = q;
        }
        if let Some(t) = tags {
            node.tags = t;
        }
        if let Some(u) = ueberholt {
            node.ueberholt = u;
        }
        if let Some(e) = erledigt {
            node.erledigt = e;
        }
    }
    recompute_scores(&mut graph);
    save(knowledge_dir, &graph)?;
    Ok(graph.nodes.into_iter().find(|n| n.id == node_id).expect("just updated"))
}

/// Delete a node and every edge touching it.
pub fn delete_node(knowledge_dir: &Path, node_id: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    let before = graph.nodes.len();
    graph.nodes.retain(|n| n.id != node_id);
    if graph.nodes.len() == before {
        return Err(Error::msg(format!("node not found: {node_id}")));
    }
    graph.edges.retain(|e| e.von != node_id && e.zu != node_id);
    recompute_scores(&mut graph);
    save(knowledge_dir, &graph)
}

/// Delete a single edge by id.
pub fn delete_edge(knowledge_dir: &Path, edge_id: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    let before = graph.edges.len();
    graph.edges.retain(|e| e.id != edge_id);
    if graph.edges.len() == before {
        return Err(Error::msg(format!("edge not found: {edge_id}")));
    }
    recompute_scores(&mut graph);
    save(knowledge_dir, &graph)
}

// ===================== Documents (citable sources) =====================

/// Subfolders under the project-meta dir that are tool-internal (never user
/// documents).
const INTERNAL_DIRS: &[&str] = &[
    "sessions",
    "knowledge",
    "session-types",
    "tasks",
    "agent-teams",
    "build",
    "rewind",
];

/// Scan user document folders under `scan_dir` for markdown and index them as
/// citable sources. Auto-discovers any NON-internal subfolder (adr, specs,
/// features, notes, docs, …). Pure read; returns sources sorted by path.
///
/// NOTE: unlike the rest of the store API (which takes the `knowledge_dir`), this
/// scans the surrounding project-meta directory the documents live in. The caller
/// passes that directory directly (the `knowledge_dir` convention does not apply
/// here).
pub fn scan_documents(scan_dir: &Path) -> Vec<Quelle> {
    let project_root = scan_dir.parent().unwrap_or(scan_dir).to_path_buf();
    let mut out: Vec<Quelle> = Vec::new();
    let Ok(entries) = std::fs::read_dir(scan_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let folder = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if folder.is_empty() || INTERNAL_DIRS.contains(&folder) {
            continue;
        }
        let art = doc_art(folder);
        collect_md(&path, &project_root, &art, &mut out);
    }
    out.sort_by(|a, b| a.pfad.cmp(&b.pfad));
    out
}

fn collect_md(dir: &Path, project_root: &Path, art: &str, out: &mut Vec<Quelle>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md(&path, project_root, art, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            let rel = path.strip_prefix(project_root).unwrap_or(&path);
            let pfad = rel.to_string_lossy().replace('\\', "/");
            let titel = doc_title(&path)
                .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string());
            out.push(Quelle { id: pfad.clone(), pfad, titel, art: art.to_string() });
        }
    }
}

/// Title = the first markdown heading (within the first lines), else the filename.
fn doc_title(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines().take(40) {
        let t = line.trim();
        if let Some(h) = t.strip_prefix('#') {
            let h = h.trim_start_matches('#').trim();
            if !h.is_empty() {
                return Some(h.chars().take(120).collect());
            }
        }
    }
    None
}

/// Singularize common doc-folder names into a kind label; else the folder name.
fn doc_art(folder: &str) -> String {
    match folder.to_lowercase().as_str() {
        "features" | "feature" => "feature",
        "adr" | "adrs" | "decisions" => "adr",
        "specs" | "spec" => "spec",
        "notes" | "note" => "note",
        "docs" | "doc" | "documentation" => "doc",
        other => return other.to_string(),
    }
    .to_string()
}

/// Replace the persisted document index (called after a scan).
pub fn set_quellen(knowledge_dir: &Path, quellen: Vec<Quelle>) -> Result<()> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    graph.quellen = quellen;
    save(knowledge_dir, &graph)
}

/// Resolve a node reference: an exact id, else a UNIQUE case-insensitive substring
/// of `inhalt` (link-by-title). Ambiguous or missing → Err with guidance.
pub fn resolve_ref(graph: &KnowledgeGraph, r: &str) -> std::result::Result<String, String> {
    let r = r.trim();
    if graph.nodes.iter().any(|n| n.id == r) {
        return Ok(r.to_string());
    }
    let rl = r.to_lowercase();
    let matches: Vec<&Node> =
        graph.nodes.iter().filter(|n| n.inhalt.to_lowercase().contains(&rl)).collect();
    match matches.len() {
        1 => Ok(matches[0].id.clone()),
        0 => Err(format!(
            "no node matches '{r}' — use an id from query_knowledge or a unique title fragment"
        )),
        _ => {
            let cand: Vec<String> = matches
                .iter()
                .take(5)
                .map(|n| format!("{} ('{}…')", n.id, n.inhalt.chars().take(40).collect::<String>()))
                .collect();
            Err(format!("'{r}' is ambiguous ({} matches): {}", matches.len(), cand.join("; ")))
        }
    }
}

/// Whitespace/case-normalized text, for duplicate detection.
fn norm_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// An existing node of the SAME type whose statement is (normalized) identical —
/// the basis for idempotent dedup-on-record.
pub fn find_duplicate(graph: &KnowledgeGraph, typ: NodeType, inhalt: &str) -> Option<String> {
    let target = norm_text(inhalt);
    graph
        .nodes
        .iter()
        .find(|n| n.typ == typ && norm_text(&n.inhalt) == target)
        .map(|n| n.id.clone())
}

/// Merge node `from_id` INTO `into_id`: re-point its edges, union its tags +
/// quelle_ids onto the target, drop the source. Self-loops and duplicate edges
/// are removed. Returns the surviving (target) node.
pub fn merge_nodes(knowledge_dir: &Path, from_id: &str, into_id: &str) -> Result<Node> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    if from_id == into_id {
        return Err(Error::msg("merge: 'from' and 'into' are the same".to_string()));
    }
    if !graph.nodes.iter().any(|n| n.id == into_id) {
        return Err(Error::msg(format!("merge target not found: {into_id}")));
    }
    let from = graph
        .nodes
        .iter()
        .find(|n| n.id == from_id)
        .cloned()
        .ok_or_else(|| Error::msg(format!("merge source not found: {from_id}")))?;
    {
        let into = graph.nodes.iter_mut().find(|n| n.id == into_id).expect("target exists");
        for t in from.tags {
            if !into.tags.contains(&t) {
                into.tags.push(t);
            }
        }
        for q in from.quelle_ids {
            if !into.quelle_ids.contains(&q) {
                into.quelle_ids.push(q);
            }
        }
    }
    for e in &mut graph.edges {
        if e.von == from_id {
            e.von = into_id.to_string();
        }
        if e.zu == from_id {
            e.zu = into_id.to_string();
        }
    }
    graph.edges.retain(|e| e.von != e.zu);
    let mut seen = HashSet::new();
    graph.edges.retain(|e| seen.insert((e.von.clone(), e.zu.clone(), e.polaritaet.clone())));
    graph.nodes.retain(|n| n.id != from_id);
    recompute_scores(&mut graph);
    save(knowledge_dir, &graph)?;
    Ok(graph.nodes.into_iter().find(|n| n.id == into_id).expect("target survives"))
}

/// Stamp a node's origin session (provenance). No-op if the node is gone; never
/// touches the score (so no recompute). Best-effort from the record path.
pub fn set_origin_session(knowledge_dir: &Path, node_id: &str, session_id: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = load(knowledge_dir);
    if let Some(n) = graph.nodes.iter_mut().find(|n| n.id == node_id) {
        n.session_id = Some(session_id.to_string());
        save(knowledge_dir, &graph)?;
    }
    Ok(())
}

/// Deterministic "tidy-up" signals over the graph — NO semantic intelligence,
/// just cheap structural checks. Surfaced to Claude (via IPC tool results) so
/// IT does the actual bundling / contradiction-resolution. See feature doc
/// (Approach A). Capped to avoid flooding.
pub fn hygiene_hints(graph: &KnowledgeGraph) -> Vec<String> {
    let mut hints = Vec::new();

    // 1. Evidence leaves not yet linked to any insight (candidates to bundle).
    let linked: HashSet<&str> = graph.edges.iter().map(|e| e.zu.as_str()).collect();
    let unlinked: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.typ,
                NodeType::Observation | NodeType::Research | NodeType::Hypothesis
            )
        })
        .filter(|n| !linked.contains(n.id.as_str()))
        .collect();
    if unlinked.len() >= 2 {
        let sample: Vec<String> = unlinked
            .iter()
            .take(4)
            .map(|n| format!("\"{}\"", truncate(&n.inhalt, 50)))
            .collect();
        hints.push(format!(
            "{} pieces of evidence are not attached to any insight ({}{}). Check whether they can be bundled into an insight (record_knowledge typ=insight, then link to the evidence).",
            unlinked.len(),
            sample.join(", "),
            if unlinked.len() > 4 { ", …" } else { "" }
        ));
    }

    // 2. Questions with competing values.
    for f in &graph.fragen {
        if f.werte.len() >= 2 {
            hints.push(format!(
                "Question \"{}\" has competing values ({}). If one answer holds, record it (weight/insight); otherwise it stays disputed.",
                truncate(&f.inhalt, 50),
                f.werte.join(" vs ")
            ));
        }
    }

    // 3. A node supported AND contradicted at once → an open conflict.
    let mut pol: HashMap<&str, (bool, bool)> = HashMap::new();
    for e in &graph.edges {
        let entry = pol.entry(e.von.as_str()).or_insert((false, false));
        if e.polaritaet == "contradicts" || e.polaritaet == "widerspricht" {
            entry.1 = true;
        } else {
            entry.0 = true;
        }
    }
    for (von, (has_pro, has_con)) in pol {
        if has_pro && has_con {
            if let Some(n) = graph.nodes.iter().find(|n| n.id == von) {
                hints.push(format!(
                    "\"{}\" has BOTH supporting AND contradicting evidence — conflict visible; assess or resolve it.",
                    truncate(&n.inhalt, 50)
                ));
            }
        }
    }

    // 4. Likely duplicates (identical normalized content).
    let mut by_norm: HashMap<String, usize> = HashMap::new();
    for n in &graph.nodes {
        *by_norm.entry(n.inhalt.trim().to_lowercase()).or_insert(0) += 1;
    }
    for n in &graph.nodes {
        let key = n.inhalt.trim().to_lowercase();
        if by_norm.get(&key).copied().unwrap_or(0) >= 2 {
            hints.push(format!(
                "Possible duplicate: \"{}\" — merge or link instead of duplicating.",
                truncate(&n.inhalt, 50)
            ));
            by_norm.insert(key, 0); // emit once per duplicate group
        }
    }

    hints.truncate(6);
    hints
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ko-kstore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_missing_is_empty() {
        let dir = temp_dir();
        let g = load(&dir);
        assert!(g.nodes.is_empty() && g.edges.is_empty() && g.fragen.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_node_roundtrips_with_default_basis_score() {
        let dir = temp_dir();
        let n = add_node(&dir, NodeType::Observation, "claude -p geht nicht".into(), "warum", None, "session").unwrap();
        assert_eq!(n.typ, NodeType::Observation);
        assert_eq!(n.basis_score, 0.90);
        assert_eq!(n.score, 0.90); // stub: score == basis_score
        assert_eq!(n.herkunft, "session");

        let g = query(&dir);
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, n.id);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_node_honors_explicit_basis_score() {
        let dir = temp_dir();
        let n = add_node(&dir, NodeType::Hypothesis, "x".into(), "warum", Some(0.42), "manual").unwrap();
        assert_eq!(n.basis_score, 0.42);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_edge_links_and_rejects_cycles() {
        let dir = temp_dir();
        let a = add_node(&dir, NodeType::Insight, "E".into(), "warum", None, "manual").unwrap();
        let b = add_node(&dir, NodeType::Observation, "B".into(), "warum", None, "manual").unwrap();

        // A depends on B (A → B): fine.
        add_edge(&dir, &a.id, &b.id, "supports", None).unwrap();
        // B → A would close a cycle.
        let err = add_edge(&dir, &b.id, &a.id, "supports", None).unwrap_err();
        assert!(matches!(err, Error::Msg(_)));

        let g = query(&dir);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].polaritaet, "supports");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_edge_rejects_missing_node() {
        let dir = temp_dir();
        let a = add_node(&dir, NodeType::Insight, "E".into(), "warum", None, "manual").unwrap();
        let err = add_edge(&dir, &a.id, "nope", "supports", None).unwrap_err();
        assert!(matches!(err, Error::Msg(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn polaritaet_is_normalized() {
        assert_eq!(normalize_polaritaet("+").unwrap(), "supports");
        assert_eq!(normalize_polaritaet("supports").unwrap(), "supports");
        assert_eq!(normalize_polaritaet("contradicts").unwrap(), "contradicts");
        assert_eq!(normalize_polaritaet("-").unwrap(), "contradicts");
        assert_eq!(normalize_polaritaet("replaces").unwrap(), "replaces");
        // Legacy German inputs still normalize to the English codes (back-compat).
        assert_eq!(normalize_polaritaet("stützt").unwrap(), "supports");
        assert_eq!(normalize_polaritaet("widerspricht").unwrap(), "contradicts");
        assert_eq!(normalize_polaritaet("ersetzt").unwrap(), "replaces");
        assert!(normalize_polaritaet("maybe").is_err());
    }

    #[test]
    fn add_fact_creates_question_and_registers_value() {
        let dir = temp_dir();
        let f1 = add_fact(&dir, "Welche Python-Version?", "3.12", "pyproject sagt 3.12".into(), "woher", None, "session").unwrap();
        assert_eq!(f1.typ, NodeType::Fact);
        assert_eq!(f1.wert.as_deref(), Some("3.12"));
        assert_eq!(f1.basis_score, 0.95);

        // Second fact for a different value of the SAME question reuses the frage.
        let _f2 = add_fact(&dir, "Welche Python-Version?", "3.11", "CI nutzt 3.11".into(), "woher", None, "session").unwrap();

        let g = query(&dir);
        assert_eq!(g.fragen.len(), 1, "same question reused");
        let frage = &g.fragen[0];
        assert_eq!(frage.werte.len(), 2);
        assert!(frage.werte.contains(&"3.12".to_string()) && frage.werte.contains(&"3.11".to_string()));
        assert_eq!(g.nodes.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- hygiene_hints (deterministic tidy-up signals) ----

    fn mk(id: &str, typ: NodeType, inhalt: &str) -> Node {
        Node {
            id: id.into(),
            typ,
            inhalt: inhalt.into(),
            begruendung: "weil test".into(),
            datum: "2026-06-04T00:00:00Z".into(),
            basis_score: 0.5,
            score: 0.5,
            status: "unsupported".into(),
            herkunft: "session".into(),
            frage_id: None,
            wert: None,
            quelle_ids: vec![],
            session_id: None,
            tags: vec![],
            ueberholt: false,
            erledigt: false,
        }
    }
    fn ed(von: &str, zu: &str, pol: &str) -> Edge {
        Edge { id: format!("{von}-{zu}"), von: von.into(), zu: zu.into(), polaritaet: pol.into(), gewicht: 1.0 }
    }

    #[test]
    fn hygiene_flags_unlinked_evidence() {
        let g = KnowledgeGraph {
            nodes: vec![mk("a", NodeType::Observation, "x"), mk("b", NodeType::Research, "y")],
            edges: vec![],
            fragen: vec![],
            quellen: vec![],
        };
        assert!(hygiene_hints(&g).iter().any(|s| s.contains("not attached to any insight")));
    }

    #[test]
    fn hygiene_no_unlinked_when_bundled() {
        let g = KnowledgeGraph {
            nodes: vec![
                mk("E", NodeType::Insight, "insight"),
                mk("a", NodeType::Observation, "x"),
                mk("b", NodeType::Research, "y"),
            ],
            edges: vec![ed("E", "a", "supports"), ed("E", "b", "supports")],
            fragen: vec![],
            quellen: vec![],
        };
        assert!(!hygiene_hints(&g).iter().any(|s| s.contains("not attached to any insight")));
    }

    #[test]
    fn hygiene_flags_competing_question() {
        let g = KnowledgeGraph {
            nodes: vec![],
            edges: vec![],
            fragen: vec![Frage {
                id: "q".into(),
                inhalt: "Py version?".into(),
                werte: vec!["3.11".into(), "3.12".into()],
            }],
            quellen: vec![],
        };
        assert!(hygiene_hints(&g).iter().any(|s| s.contains("competing values")));
    }

    #[test]
    fn hygiene_flags_conflicting_support() {
        let g = KnowledgeGraph {
            nodes: vec![
                mk("E", NodeType::Insight, "claim"),
                mk("a", NodeType::Observation, "pro"),
                mk("b", NodeType::Observation, "con"),
            ],
            edges: vec![ed("E", "a", "supports"), ed("E", "b", "contradicts")],
            fragen: vec![],
            quellen: vec![],
        };
        assert!(hygiene_hints(&g).iter().any(|s| s.contains("conflict visible")));
    }

    #[test]
    fn hygiene_flags_duplicates() {
        let g = KnowledgeGraph {
            nodes: vec![mk("a", NodeType::Observation, "Same Thing"), mk("b", NodeType::Observation, "same thing")],
            edges: vec![],
            fragen: vec![],
            quellen: vec![],
        };
        assert!(hygiene_hints(&g).iter().any(|s| s.contains("Possible duplicate")));
    }

    #[test]
    fn hygiene_empty_graph_no_hints() {
        assert!(hygiene_hints(&KnowledgeGraph::default()).is_empty());
    }

    // ---- curation ops ----

    #[test]
    fn update_node_changes_fields() {
        let dir = temp_dir();
        let n = add_node(&dir, NodeType::Hypothesis, "alt".into(), "warum", None, "manual").unwrap();
        let upd = update_node(&dir, &n.id, Some("neu".into()), Some("warum neu".into()), Some(NodeType::Observation), Some(0.8), None, None, None, None).unwrap();
        assert_eq!(upd.inhalt, "neu");
        assert_eq!(upd.typ, NodeType::Observation);
        assert_eq!(upd.basis_score, 0.8);
        assert_eq!(upd.score, 0.8); // stub: score == basis_score
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_node_removes_node_and_its_edges() {
        let dir = temp_dir();
        let a = add_node(&dir, NodeType::Insight, "E".into(), "warum", None, "manual").unwrap();
        let b = add_node(&dir, NodeType::Observation, "B".into(), "warum", None, "manual").unwrap();
        add_edge(&dir, &a.id, &b.id, "supports", None).unwrap();
        delete_node(&dir, &b.id).unwrap();
        let g = query(&dir);
        assert_eq!(g.nodes.len(), 1);
        assert!(g.edges.is_empty(), "edges touching a deleted node must be removed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_edge_removes_only_that_edge() {
        let dir = temp_dir();
        let a = add_node(&dir, NodeType::Insight, "E".into(), "warum", None, "manual").unwrap();
        let b = add_node(&dir, NodeType::Observation, "B".into(), "warum", None, "manual").unwrap();
        let e = add_edge(&dir, &a.id, &b.id, "supports", None).unwrap();
        delete_edge(&dir, &e.id).unwrap();
        let g = query(&dir);
        assert_eq!(g.nodes.len(), 2, "nodes stay");
        assert!(g.edges.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_node_missing_errs() {
        let dir = temp_dir();
        assert!(matches!(delete_node(&dir, "nope").unwrap_err(), Error::Msg(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- scoring engine (concept §4) ----

    fn leaf(id: &str, typ: NodeType, basis: f64) -> Node {
        let mut n = mk(id, typ, id);
        n.basis_score = basis;
        n
    }
    fn fakt(id: &str, frage: &str, wert: &str, basis: f64) -> Node {
        let mut n = mk(id, NodeType::Fact, &format!("fakt {wert}"));
        n.basis_score = basis;
        n.frage_id = Some(frage.into());
        n.wert = Some(wert.into());
        n
    }
    fn by<'a>(g: &'a KnowledgeGraph, id: &str) -> &'a Node {
        g.nodes.iter().find(|n| n.id == id).unwrap()
    }

    #[test]
    fn engine_worked_example_gestuetzt() {
        let mut g = KnowledgeGraph {
            nodes: vec![
                mk("D1", NodeType::Decision, "decide"),
                mk("E1", NodeType::Insight, "insight"),
                leaf("B1", NodeType::Observation, 0.9),
                leaf("R1", NodeType::Research, 0.7),
                leaf("V1", NodeType::Hypothesis, 0.3),
            ],
            edges: vec![
                ed("D1", "E1", "supports"),
                ed("E1", "B1", "supports"),
                ed("E1", "R1", "supports"),
                ed("E1", "V1", "supports"),
            ],
            fragen: vec![],
            quellen: vec![],
        };
        recompute_scores(&mut g);
        assert_eq!(by(&g, "B1").score, 0.9);
        assert_eq!(by(&g, "B1").status, "supported"); // uncontested leaf
        assert!((by(&g, "E1").score - 1.9 / 2.9).abs() < 1e-6, "E1={}", by(&g, "E1").score);
        assert_eq!(by(&g, "E1").status, "supported");
        // D1 (decision) keeps a groundedness SCORE below E1, but its STATUS is
        // the lifecycle "active" (decisions aren't belief-scored as supported/unsupported).
        assert!(by(&g, "D1").score > 0.0 && by(&g, "D1").score < by(&g, "E1").score);
        assert_eq!(by(&g, "D1").status, "active");
    }

    #[test]
    fn engine_contradiction_makes_umstritten() {
        let mut g = KnowledgeGraph {
            nodes: vec![
                mk("E1", NodeType::Insight, "insight"),
                leaf("B1", NodeType::Observation, 0.9),
                leaf("R1", NodeType::Research, 0.7),
                leaf("V1", NodeType::Hypothesis, 0.3),
                leaf("B2", NodeType::Observation, 0.9),
            ],
            edges: vec![
                ed("E1", "B1", "supports"),
                ed("E1", "R1", "supports"),
                ed("E1", "V1", "supports"),
                ed("E1", "B2", "contradicts"),
            ],
            fragen: vec![],
            quellen: vec![],
        };
        recompute_scores(&mut g);
        // pro=1.9, con=0.9 → score 1.9/3.8 = 0.5, r=0.9/2.8≈0.32 → disputed
        assert!((by(&g, "E1").score - 0.5).abs() < 1e-6, "E1={}", by(&g, "E1").score);
        assert_eq!(by(&g, "E1").status, "disputed");
    }

    #[test]
    fn engine_competing_facts_umstritten() {
        let mut g = KnowledgeGraph {
            nodes: vec![fakt("f1", "Q", "3.11", 0.95), fakt("f2", "Q", "3.12", 0.95)],
            edges: vec![],
            fragen: vec![Frage { id: "Q".into(), inhalt: "version?".into(), werte: vec!["3.11".into(), "3.12".into()] }],
            quellen: vec![],
        };
        recompute_scores(&mut g);
        for n in &g.nodes {
            assert_eq!(n.status, "disputed");
            assert!((n.score - 0.95 / 2.9).abs() < 1e-6, "score={}", n.score);
        }
    }

    #[test]
    fn engine_consensus_facts_gestuetzt() {
        let mut g = KnowledgeGraph {
            nodes: vec![fakt("f1", "Q", "3.12", 0.95), fakt("f2", "Q", "3.12", 0.95)],
            edges: vec![],
            fragen: vec![Frage { id: "Q".into(), inhalt: "version?".into(), werte: vec!["3.12".into()] }],
            quellen: vec![],
        };
        recompute_scores(&mut g);
        for n in &g.nodes {
            assert_eq!(n.status, "supported");
            assert!((n.score - 1.9 / 2.9).abs() < 1e-6, "score={}", n.score);
        }
    }

    #[test]
    fn engine_insight_without_evidence_is_unbelegt() {
        let mut g = KnowledgeGraph {
            nodes: vec![mk("E", NodeType::Insight, "x")],
            edges: vec![],
            fragen: vec![],
            quellen: vec![],
        };
        recompute_scores(&mut g);
        assert_eq!(by(&g, "E").status, "unsupported");
        assert_eq!(by(&g, "E").score, 0.0);
    }

    #[test]
    fn engine_standalone_fakt_scores_basis() {
        // A Fakt without a Frage (optional now) scores like a leaf = its basis.
        let mut f = mk("F", NodeType::Fact, "Python is 3.12");
        f.basis_score = 0.95;
        let mut g = KnowledgeGraph { nodes: vec![f], edges: vec![], fragen: vec![], quellen: vec![] };
        recompute_scores(&mut g);
        assert_eq!(g.nodes[0].score, 0.95);
        assert_eq!(g.nodes[0].status, "supported");
    }

    // ---- query filter (read-side) ----

    #[test]
    fn filter_by_keyword_includes_neighbor() {
        let g = KnowledgeGraph {
            nodes: vec![
                mk("E", NodeType::Insight, "rain insight"),
                mk("B", NodeType::Observation, "street is wet"),
                mk("X", NodeType::Observation, "unrelated stuff"),
            ],
            edges: vec![ed("E", "B", "supports")],
            fragen: vec![],
            quellen: vec![],
        };
        let view = apply_filter(&g, &QueryFilter { q: Some("rain".into()), ..Default::default() });
        let ids: Vec<&str> = view.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"E"), "keyword match");
        assert!(ids.contains(&"B"), "1-hop neighbour kept");
        assert!(!ids.contains(&"X"), "unrelated excluded");
        assert_eq!(view.edges.len(), 1);
    }

    #[test]
    fn filter_by_type() {
        let g = KnowledgeGraph {
            nodes: vec![mk("E", NodeType::Insight, "i"), mk("B", NodeType::Observation, "b")],
            edges: vec![],
            fragen: vec![],
            quellen: vec![],
        };
        let view = apply_filter(&g, &QueryFilter { typ: Some(NodeType::Insight), ..Default::default() });
        assert_eq!(view.nodes.len(), 1);
        assert_eq!(view.nodes[0].id, "E");
    }

    #[test]
    fn filter_by_status() {
        let mut a = mk("A", NodeType::Observation, "a");
        a.status = "disputed".into();
        let g = KnowledgeGraph { nodes: vec![a, mk("B", NodeType::Observation, "b")], edges: vec![], fragen: vec![], quellen: vec![] };
        let view = apply_filter(&g, &QueryFilter { status: Some("disputed".into()), ..Default::default() });
        assert_eq!(view.nodes.len(), 1);
        assert_eq!(view.nodes[0].id, "A");
    }

    #[test]
    fn filter_limit_keeps_top_by_score() {
        let mut a = mk("A", NodeType::Observation, "keyword a");
        a.score = 0.9;
        let mut b = mk("B", NodeType::Observation, "keyword b");
        b.score = 0.5;
        let mut c = mk("C", NodeType::Observation, "keyword c");
        c.score = 0.1;
        let g = KnowledgeGraph { nodes: vec![a, b, c], edges: vec![], fragen: vec![], quellen: vec![] };
        let view = apply_filter(&g, &QueryFilter { q: Some("keyword".into()), limit: Some(2), ..Default::default() });
        let ids: Vec<&str> = view.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"A") && ids.contains(&"B") && !ids.contains(&"C"), "top-2 by score, got {ids:?}");
    }

    #[test]
    fn filter_inactive_returns_all() {
        let g = KnowledgeGraph {
            nodes: vec![mk("A", NodeType::Observation, "a"), mk("B", NodeType::Observation, "b")],
            edges: vec![],
            fragen: vec![],
            quellen: vec![],
        };
        assert_eq!(apply_filter(&g, &QueryFilter::default()).nodes.len(), 2);
    }

    #[test]
    fn filter_by_tags_matches_any() {
        let mut a = mk("A", NodeType::Observation, "a");
        a.tags = vec!["voxel".into()];
        let mut b = mk("B", NodeType::Observation, "b");
        b.tags = vec!["pathfinding".into()];
        let g = KnowledgeGraph { nodes: vec![a, b], edges: vec![], fragen: vec![], quellen: vec![] };
        let f = QueryFilter { tags: vec!["VOXEL".into()], ..Default::default() };
        let view = apply_filter(&g, &f);
        assert_eq!(view.nodes.len(), 1);
        assert_eq!(view.nodes[0].id, "A");
    }

    #[test]
    fn get_subgraph_pulls_neighbourhood_to_depth() {
        let g = KnowledgeGraph {
            nodes: vec![
                mk("E", NodeType::Insight, "claim"),
                mk("B", NodeType::Observation, "obs"),
                mk("X", NodeType::Hypothesis, "far"),
            ],
            edges: vec![Edge {
                id: "e1".into(),
                von: "E".into(),
                zu: "B".into(),
                polaritaet: "supports".into(),
                gewicht: 1.0,
            }],
            fragen: vec![],
            quellen: vec![],
        };
        let sub = get_subgraph(&g, &["E".into()], 1);
        let ids: Vec<&str> = sub.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"E") && ids.contains(&"B"), "{ids:?}");
        assert!(!ids.contains(&"X"));
    }

    // ---- decision lifecycle ----

    #[test]
    fn entscheidung_without_evidence_is_aktiv_not_unbelegt() {
        let mut g = KnowledgeGraph {
            nodes: vec![mk("D", NodeType::Decision, "use X")],
            edges: vec![],
            fragen: vec![],
            quellen: vec![],
        };
        recompute_scores(&mut g);
        assert_eq!(g.nodes[0].status, "active");
    }

    #[test]
    fn ersetzt_edge_retires_old_keeps_new_aktiv() {
        let mut g = KnowledgeGraph {
            nodes: vec![
                mk("ALT", NodeType::Decision, "old"),
                mk("NEU", NodeType::Decision, "new"),
            ],
            edges: vec![Edge {
                id: "e".into(),
                von: "NEU".into(),
                zu: "ALT".into(),
                polaritaet: "replaces".into(),
                gewicht: 1.0,
            }],
            fragen: vec![],
            quellen: vec![],
        };
        recompute_scores(&mut g);
        let st = |id: &str| g.nodes.iter().find(|n| n.id == id).unwrap().status.clone();
        assert_eq!(st("ALT"), "superseded");
        assert_eq!(st("NEU"), "active");
    }

    #[test]
    fn ueberholt_flag_retires_any_node() {
        let mut a = mk("A", NodeType::Insight, "claim");
        a.ueberholt = true;
        let mut g = KnowledgeGraph { nodes: vec![a], edges: vec![], fragen: vec![], quellen: vec![] };
        recompute_scores(&mut g);
        assert_eq!(g.nodes[0].status, "superseded");
    }

    #[test]
    fn erledigt_flag_marks_done_ueberholt_wins() {
        let mut d = mk("D", NodeType::Decision, "do X");
        d.erledigt = true;
        let mut g = KnowledgeGraph { nodes: vec![d], edges: vec![], fragen: vec![], quellen: vec![] };
        recompute_scores(&mut g);
        assert_eq!(g.nodes[0].status, "done");
        // überholt takes precedence over erledigt.
        g.nodes[0].ueberholt = true;
        recompute_scores(&mut g);
        assert_eq!(g.nodes[0].status, "superseded");
    }

    // ---- resolve / dedup / merge ----

    #[test]
    fn resolve_ref_by_id_and_unique_title() {
        let g = KnowledgeGraph {
            nodes: vec![mk("n1", NodeType::Observation, "uses Phaser 4")],
            edges: vec![],
            fragen: vec![],
            quellen: vec![],
        };
        assert_eq!(resolve_ref(&g, "n1").unwrap(), "n1");
        assert_eq!(resolve_ref(&g, "phaser").unwrap(), "n1");
        assert!(resolve_ref(&g, "nope").is_err());
    }

    #[test]
    fn find_duplicate_matches_normalized_same_type_only() {
        let g = KnowledgeGraph {
            nodes: vec![mk("n1", NodeType::Fact, "Uses   Python 3.12")],
            edges: vec![],
            fragen: vec![],
            quellen: vec![],
        };
        assert_eq!(find_duplicate(&g, NodeType::Fact, "uses python 3.12").as_deref(), Some("n1"));
        assert_eq!(find_duplicate(&g, NodeType::Observation, "uses python 3.12"), None);
    }

    #[test]
    fn merge_nodes_repoints_edges_and_unions_tags() {
        let dir = temp_dir();
        let keep = add_node(&dir, NodeType::Insight, "insight".into(), "warum", None, "manual").unwrap();
        let dup = add_node(&dir, NodeType::Insight, "insight dup".into(), "warum", None, "manual").unwrap();
        let ev = add_node(&dir, NodeType::Observation, "obs".into(), "warum", None, "manual").unwrap();
        add_edge(&dir, &dup.id, &ev.id, "supports", None).unwrap();
        update_node(&dir, &dup.id, None, None, None, None, None, Some(vec!["voxel".into()]), None, None).unwrap();
        merge_nodes(&dir, &dup.id, &keep.id).unwrap();
        let g = query(&dir);
        assert!(g.nodes.iter().all(|n| n.id != dup.id), "dup removed");
        assert!(g.edges.iter().any(|e| e.von == keep.id && e.zu == ev.id), "edge re-pointed");
        assert!(g.nodes.iter().find(|n| n.id == keep.id).unwrap().tags.contains(&"voxel".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- document scan ----

    #[test]
    fn scan_documents_indexes_md_by_folder_and_skips_internal() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("adr")).unwrap();
        std::fs::write(dir.join("adr").join("001-foo.md"), "# Decision Foo\n\nbody").unwrap();
        // internal dir → ignored
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::write(dir.join("sessions").join("x.md"), "# nope").unwrap();

        let docs = scan_documents(&dir);
        assert_eq!(docs.len(), 1, "{docs:?}");
        assert_eq!(docs[0].art, "adr");
        assert_eq!(docs[0].titel, "Decision Foo");
        assert!(docs[0].pfad.ends_with("adr/001-foo.md"), "{}", docs[0].pfad);
        assert_eq!(docs[0].id, docs[0].pfad);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_quellen_persists() {
        let dir = temp_dir();
        let q = Quelle { id: ".claude/adr/1.md".into(), pfad: ".claude/adr/1.md".into(), titel: "T".into(), art: "adr".into() };
        set_quellen(&dir, vec![q]).unwrap();
        let g = query(&dir);
        assert_eq!(g.quellen.len(), 1);
        assert_eq!(g.quellen[0].titel, "T");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- .md store: node frontmatter round-trip ----

    fn full_node(id: &str, typ: NodeType) -> Node {
        Node {
            id: id.into(),
            typ,
            // multiline content with the body separator-ish text + special chars to
            // stress the round-trip (quotes, unicode, leading/trailing spaces).
            inhalt: "Zeile 1: \"claude -p\" geht nicht.\nZeile 2: ümläuts & <html>\n  eingerückt".into(),
            begruendung: "weil:\n- verifiziert per Hand\n- ÄÖÜ ß 😀".into(),
            datum: "2026-06-04T12:34:56+00:00".into(),
            basis_score: 0.73,
            score: 0.61,
            status: "supported".into(),
            herkunft: "session".into(),
            frage_id: Some("frage-x".into()),
            wert: Some("3.12".into()),
            quelle_ids: vec![".claude/adr/1.md".into(), ".claude/specs/s.md".into()],
            session_id: Some("sess-42".into()),
            tags: vec!["voxel".into(), "path,finding".into()],
            ueberholt: true,
            erledigt: false,
        }
    }

    fn assert_node_eq(a: &Node, b: &Node) {
        assert_eq!(a.id, b.id, "id");
        assert_eq!(a.typ, b.typ, "typ");
        assert_eq!(a.inhalt, b.inhalt, "inhalt");
        assert_eq!(a.begruendung, b.begruendung, "begruendung");
        assert_eq!(a.datum, b.datum, "datum");
        assert_eq!(a.basis_score, b.basis_score, "basis_score");
        assert_eq!(a.score, b.score, "score");
        assert_eq!(a.status, b.status, "status");
        assert_eq!(a.herkunft, b.herkunft, "herkunft");
        assert_eq!(a.frage_id, b.frage_id, "frage_id");
        assert_eq!(a.wert, b.wert, "wert");
        assert_eq!(a.quelle_ids, b.quelle_ids, "quelle_ids");
        assert_eq!(a.session_id, b.session_id, "session_id");
        assert_eq!(a.tags, b.tags, "tags");
        assert_eq!(a.ueberholt, b.ueberholt, "ueberholt");
        assert_eq!(a.erledigt, b.erledigt, "done");
    }

    #[test]
    fn node_md_roundtrips_every_field_for_each_type() {
        for typ in [
            NodeType::Decision,
            NodeType::Insight,
            NodeType::Fact,
            NodeType::Observation,
            NodeType::Research,
            NodeType::Hypothesis,
        ] {
            let n = full_node("id-1", typ);
            let md = serialize_node_md(&n);
            let parsed = parse_node_md(&md).expect("parse");
            assert_node_eq(&n, &parsed);
        }
    }

    #[test]
    fn node_md_roundtrips_minimal_optionals_absent() {
        let n = Node {
            id: "minimal".into(),
            typ: NodeType::Hypothesis,
            inhalt: "x".into(),
            begruendung: "y".into(),
            datum: "2026-01-01T00:00:00Z".into(),
            basis_score: 0.3,
            score: 0.3,
            status: "supported".into(),
            herkunft: "manual".into(),
            frage_id: None,
            wert: None,
            quelle_ids: vec![],
            session_id: None,
            tags: vec![],
            ueberholt: false,
            erledigt: false,
        };
        let parsed = parse_node_md(&serialize_node_md(&n)).expect("parse");
        assert_node_eq(&n, &parsed);
    }

    #[test]
    fn store_roundtrips_via_save_load_layout() {
        let dir = temp_dir();
        // Build a graph through the public API, then verify load_layout reproduces it.
        let a = add_node(&dir, NodeType::Insight, "insight".into(), "warum", None, "session").unwrap();
        let b = add_node(&dir, NodeType::Observation, "obs".into(), "warum", None, "session").unwrap();
        add_edge(&dir, &a.id, &b.id, "supports", None).unwrap();
        let _f = add_fact(&dir, "Frage?", "3.12", "fakt".into(), "woher", None, "session").unwrap();
        set_quellen(&dir, vec![Quelle {
            id: "p".into(),
            pfad: "p".into(),
            titel: "T".into(),
            art: "doc".into(),
        }]).unwrap();

        // Files exist in the new layout, no graph.json. (knowledge_dir-direct: the
        // dir passed IS the knowledge dir, so no intermediate "knowledge" segment.)
        assert!(!dir.join("graph.json").exists());
        assert!(dir.join("nodes").join(format!("{}.md", a.id)).exists());
        assert!(dir.join("edges.json").exists());

        let g = load_layout(&dir);
        assert_eq!(g.nodes.len(), 3);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.fragen.len(), 1);
        assert_eq!(g.quellen.len(), 1);
        // Edge/frage/quelle fields intact.
        assert_eq!(g.edges[0].polaritaet, "supports");
        assert_eq!(g.fragen[0].inhalt, "Frage?");
        assert_eq!(g.quellen[0].titel, "T");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_node_removes_its_md_file() {
        let dir = temp_dir();
        let a = add_node(&dir, NodeType::Observation, "gone".into(), "warum", None, "manual").unwrap();
        let path = dir.join("nodes").join(format!("{}.md", a.id));
        assert!(path.exists());
        delete_node(&dir, &a.id).unwrap();
        assert!(!path.exists(), "deleting a node removes its .md");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_removes_source_md_file() {
        let dir = temp_dir();
        let keep = add_node(&dir, NodeType::Insight, "keep".into(), "warum", None, "manual").unwrap();
        let dup = add_node(&dir, NodeType::Insight, "dup".into(), "warum", None, "manual").unwrap();
        let dup_path = dir.join("nodes").join(format!("{}.md", dup.id));
        assert!(dup_path.exists());
        merge_nodes(&dir, &dup.id, &keep.id).unwrap();
        assert!(!dup_path.exists(), "merged-away node's .md removed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_write_leaves_no_temp_and_replaces_fully() {
        let dir = temp_dir();
        let target = dir.join("nodes").join("z.md");
        atomic_write(&target, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        atomic_write(&target, b"second longer content").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second longer content");
        // No leftover temp files in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left behind: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_read_during_writes_sees_consistent_node() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let dir = temp_dir();
        let a = add_node(&dir, NodeType::Observation, "v0".into(), "warum", None, "session").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let writer_dir = dir.clone();
        let writer_id = a.id.clone();
        let stop_w = stop.clone();
        let writer = std::thread::spawn(move || {
            for i in 0..200 {
                update_node(&writer_dir, &writer_id, Some(format!("v{i}")), None, None, None, None, None, None, None).unwrap();
            }
            stop_w.store(true, Ordering::SeqCst);
        });
        // Reader: while the writer runs, every load must yield a parseable node
        // (never a torn/partial file → parse_node_md would return the node anyway,
        // but load_layout silently skips unparseable, so assert the node is present).
        while !stop.load(Ordering::SeqCst) {
            let g = load_layout(&dir);
            assert_eq!(g.nodes.len(), 1, "node always fully present during concurrent writes");
            assert!(g.nodes[0].inhalt.starts_with('v'), "consistent content: {}", g.nodes[0].inhalt);
        }
        writer.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}

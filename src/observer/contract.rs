//! Observer extraction contract (JSON, Observer → Rust) + the extraction prompt.
//! Pure serde, no cross-module deps.
//!
//! The headless observer LLM returns a JSON object of the shape:
//! ```json
//! {
//!   "ops": [
//!     { "action": "add", "typ": "observation|fact|insight|decision|research|hypothesis",
//!       "inhalt": "…", "begruendung": "…", "tags": ["…"], "quellen": ["<doc-id>"],
//!       "links": [ { "ziel": "<id|title>", "polaritaet": "supports|contradicts|replaces",
//!                    "als": "parent|child" } ] },
//!     { "action": "update", "id": "…", "fields": { "inhalt": "…", "ueberholt": true } },
//!     { "action": "link", "von": "<id|title>", "zu": "<id|title>", "polaritaet": "…" },
//!     { "action": "noop" }
//!   ],
//!   "rolling_summary": "compact updated summary of the session arc"
//! }
//! ```
//! `parse` is robust against chatter (prose / ```json fences) and against a single
//! malformed op poisoning the whole array: it extracts the first balanced `{…}`
//! object, deserializes `ops` per-element (dropping the ones that fail) and keeps
//! `rolling_summary` if present. On ANY failure it returns
//! `ExtractionResult::default()` — it NEVER panics.

use serde::Deserialize;

fn default_polaritaet() -> String {
    "supports".to_string()
}

fn default_als() -> String {
    "parent".to_string()
}

/// An inline link spec attached to an `add` op.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LinkSpec {
    /// Target node — an id OR a unique title substring (resolved by the engine).
    pub ziel: String,
    /// "supports" | "contradicts" | "replaces" (default "supports").
    #[serde(default = "default_polaritaet")]
    pub polaritaet: String,
    /// "parent" | "child" (default "parent").
    #[serde(default = "default_als")]
    pub als: String,
}

/// Only-set-fields patch for an `update` op. All fields optional.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct UpdateFields {
    #[serde(default)]
    pub inhalt: Option<String>,
    #[serde(default)]
    pub begruendung: Option<String>,
    #[serde(default)]
    pub typ: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub ueberholt: Option<bool>,
    #[serde(default)]
    pub erledigt: Option<bool>,
    /// Document ids (orthogonal source layer) this node is backed by. `Some(vec)`
    /// replaces the node's cited sources; `None` = leave them untouched.
    #[serde(default)]
    pub quellen: Option<Vec<String>>,
}

/// A single operation the observer wants applied to the DAG.
///
/// Internally tagged on `action`. An unknown/garbage `action` degrades to
/// [`Op::Noop`] (via `#[serde(other)]`) instead of failing the whole parse.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Op {
    /// Add a new node (the engine may re-interpret it as an update via dedup).
    Add {
        typ: String,
        inhalt: String,
        begruendung: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        links: Vec<LinkSpec>,
        /// Document ids (orthogonal source layer) this node is backed by / derived
        /// from / documented in. NOT DAG links — a source citation. Each id must be
        /// an EXACT id from the "Available project documents" input list (Rust
        /// filters out any unknown/invented id before recording).
        #[serde(default)]
        quellen: Vec<String>,
    },
    /// Update an existing node — only the set `fields`.
    Update {
        id: String,
        #[serde(default)]
        fields: UpdateFields,
    },
    /// Add an edge between two existing nodes (each an id OR a unique title).
    Link {
        von: String,
        zu: String,
        polaritaet: String,
    },
    /// Explicit no-op AND the catch-all for unknown actions / garbage.
    #[serde(other)]
    Noop,
}

/// The full deserialized observer result.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ExtractionResult {
    #[serde(default)]
    pub ops: Vec<Op>,
    #[serde(default)]
    pub rolling_summary: String,
    /// Optional one-line relevance pointer (Slice B, B-2): if the LLM judges the
    /// agent could use existing project knowledge it hasn't fetched, it emits a
    /// single-line pointer here (NOT a command). Default empty = no nudge.
    #[serde(default)]
    pub hint: String,
}

/// Find the first balanced top-level `{…}` object in `s`, respecting that braces
/// may appear inside double-quoted strings (with backslash escapes). Returns the
/// substring including the outer braces, or `None` if no balanced object is found.
fn extract_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for i in start..bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Strip ```json … ``` (or plain ``` … ```) fences if present, leaving the inner body.
fn strip_code_fences(s: &str) -> String {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        // Drop an optional language tag on the first line (e.g. "json").
        let rest = match rest.find('\n') {
            Some(nl) => &rest[nl + 1..],
            None => rest,
        };
        // Drop a trailing closing fence if present.
        if let Some(end) = rest.rfind("```") {
            return rest[..end].to_string();
        }
        return rest.to_string();
    }
    trimmed.to_string()
}

/// Why an observer response could not be turned into an [`ExtractionResult`].
///
/// The two cases are worth distinguishing when reading logs: `NoJson` means the agent
/// answered in prose — it drifted out of its extraction role; `Invalid` means it meant
/// to emit ops but produced broken JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// No JSON object in the response at all (pure prose — a drifted agent).
    NoJson,
    /// A JSON object was found but does not parse. Observed live (2026-08-04): the model
    /// opened a German quote `„` and closed it with a straight ASCII `"`, which
    /// terminated the JSON string early and cost the whole extraction.
    Invalid(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::NoJson => {
                write!(f, "no JSON object in the response (agent answered in prose?)")
            }
            ParseError::Invalid(e) => write!(f, "malformed JSON: {e}"),
        }
    }
}

/// Parse the observer's stdout into an [`ExtractionResult`], reporting failure.
///
/// Robust against chatter (prose / ```json fences) and against a single malformed op
/// poisoning the whole array — those are still tolerated. What is NOT tolerated is a
/// response that yields no usable object at all: that returns `Err` so the caller can
/// treat the pass as failed and RETRY the delta instead of dropping it. NEVER panics.
pub fn parse_checked(stdout: &str) -> Result<ExtractionResult, ParseError> {
    let unfenced = strip_code_fences(stdout);
    let json = match extract_json_object(&unfenced) {
        Some(j) => j,
        None => {
            // No BALANCED object — but that does NOT imply "no JSON attempted". An
            // unbalanced quote flips the scanner's string parity, which hides every
            // following brace from it (exactly the 2026-08-04 smart-quote case). Retry
            // on the widest `{…}` span so serde can name the real syntax error; a dump
            // saying "expected `,` or `}` at column 84" is recoverable by hand, a bare
            // "no JSON" is not.
            return Err(match (unfenced.find('{'), unfenced.rfind('}')) {
                (Some(a), Some(b)) if b > a => ParseError::Invalid(
                    serde_json::from_str::<serde_json::Value>(&unfenced[a..=b])
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "unbalanced JSON object".to_string()),
                ),
                // Genuinely no object anywhere → the agent answered in prose.
                _ => ParseError::NoJson,
            });
        }
    };

    // Parse the outer object loosely so one bad op can't poison the whole array.
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        ops: Vec<serde_json::Value>,
        #[serde(default)]
        rolling_summary: String,
        #[serde(default)]
        hint: String,
    }

    let raw: Raw =
        serde_json::from_str(json).map_err(|e| ParseError::Invalid(e.to_string()))?;

    let ops = raw
        .ops
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Op>(v).ok())
        .collect();

    Ok(ExtractionResult {
        ops,
        rolling_summary: raw.rolling_summary,
        hint: raw.hint,
    })
}

/// Tolerant wrapper around [`parse_checked`]: collapses a parse failure to an empty
/// result.
///
/// **Do not use this on the extraction path.** An empty result there is
/// indistinguishable from "the agent found nothing durable", so the caller records a
/// success and advances the watermark — silently burning the user's turns. That bug
/// cost a full session's knowledge on 2026-08-04. Use [`parse_checked`] and handle the
/// error. This wrapper remains for probes/tests where a failure genuinely does not matter.
pub fn parse(stdout: &str) -> ExtractionResult {
    parse_checked(stdout).unwrap_or_default()
}

/// The extraction prompt handed to the headless observer LLM (Slice A, Schritt 7).
///
/// 3rd-person observer variant of the knowledge rules. Written in English for the
/// public release. CRITICAL: the language of THIS prompt does NOT dictate the
/// language of the content the observer records — every item is recorded in the
/// language of the conversation being observed (see the first rule). Per-project
/// overridable later (Slice B / tuning).
pub const EXTRACTION_PROMPT: &str = "You are observing a dialogue between a user and a coding agent. Your job is to extract **durable, project-wide** knowledge from it and translate that into structured operations (ADD/UPDATE/LINK) on a scored evidence DAG. You do NOT answer the user and you do NOT steer the agent — you only maintain the knowledge.\n\
\n\
LANGUAGE RULE (read first): Record every item — its statement (inhalt), its rationale (begruendung), any question/value text — AND your rolling summary in the SAME language as the conversation you are observing. Do NOT translate the content to English. These instructions are written in English; that must NOT change the language of what you record. The field KEYS and the schema VALUES below (the typ/status/polaritaet codes) are fixed English identifiers and stay exactly as written regardless of the conversation's language; only the free-text human content follows the conversation.\n\
\n\
RULES:\n\
(1) Capture ONLY durable, project-wide knowledge. Do NOT store transient, session-local process events or control flow — e.g. 'this step only does X', 'Y comes later', 'another agent handles Z', 'user says go', 'tuning skipped', current task scoping. Such things belong in the transcript, not the graph. If a message ALSO contains a durable fact or decision (e.g. a data format), capture ONLY that part. Also NOT knowledge: progress/status reports ('X committed', 'build is green', 'tests pass now', 'state of the build agent', 'step N done'), and notes about the tooling/environment of this chat rather than the project.\n\
(1b) PRIORITY — what makes this graph worth more than the project docs: the WHY behind a choice (trade-off, cause, goal) and approaches that were TRIED or CONSIDERED and REJECTED/FAILED, together with the reason. Capture these even when the change itself looks small; the current state alone is usually already in the code and docs.\n\
(2) Choose the node type by SOURCE and CERTAINTY (not by topic), inferred from the phrasing: fact = certain hard info ('it is X', from the user or a database), observation = directly observed / 'looks like', research = looked up externally, hypothesis = uncertain/hedged ('I think', 'probably'), insight = a conclusion you derived yourself, decision = a choice that was made. The user is NOT omniscient — judge by the phrasing, not by the mere fact that it was said. If you are unsure how certain something is or where it comes from, do NOT extract it (when in doubt, leave it out rather than guess).\n\
(3) Every ADD operation REQUIRES a 'begruendung' (otherwise the op is dropped), type-adaptive: for observation/research/hypothesis the question it answers (as a question); for insight the rationale/derivation; for decision its purpose (what it is for); for fact the source it comes from.\n\
(4) Build a GRAPH, not a flat list: when several observation/research/hypothesis nodes point at a higher-level insight, create an insight and connect the evidence (polaritaet 'supports', or 'contradicts' for counter-evidence); ground a decision on what supports it. Create these edges INLINE via the 'links' field of the ADD op (ziel = id OR unique title substring, polaritaet, als 'parent'|'child').\n\
(5) Decisions are a lifecycle, not an evidence vote: status is active / done / superseded, and a decision is not 'weak' just because it lacks evidence. When a choice changes, link the NEW decision to the old one via a LINK op with polaritaet='replaces' (old → superseded) — NEVER leave contradicting decision nodes side by side. If a decision is simply CARRIED OUT (a completed step), mark it via UPDATE with fields.erledigt=true. Correct or retire wrong/outdated nodes via UPDATE with fields.ueberholt=true instead of piling up duplicates.\n\
(6) DEDUP awareness: you are given an excerpt of the existing DAG (nodes with id/title/tags). Check it BEFORE creating anything new. If the thought already exists (even worded differently), use UPDATE (refine the node) or LINK (connect) instead of a blind ADD. Extend earlier insight nodes when new evidence arrives instead of creating new ones. If an excerpt node states an OLDER version of what you are about to record (other values, other approach, 'still open' vs. 'fixed'), never leave both side by side: for a decision ADD the new one with a 'replaces' link to the old; otherwise UPDATE the old one, or ADD the new one and mark the old via UPDATE fields.ueberholt=true. When something is refuted, ADD/LINK the counter-evidence with polaritaet 'contradicts' so the reason stays traceable.\n\
(7) Tag every node via 'tags' with its topic/area (subsystem or work-stream, e.g. 'pathfinding', 'voxel'), so later work can retrieve just that stream.\n\
(8) Keep each node to ONE concise statement. Also capture one-off/dated specifics (the timestamp is set automatically). Skip pure pleasantries.\n\
(9) DOCUMENTS AS SOURCES: the input is prefixed with an 'Available project documents' section that lists each document with its exact 'id' and title. If an extracted piece of knowledge is backed by / derived from / documented in one of these documents (e.g. a 'fact' whose source is a feature doc, or a 'decision' documented in F00x), cite the relevant document 'id(s)' in the op's 'quellen' array — using the EXACT id from the list, NEVER invented. 'quellen' are SOURCES (an orthogonal evidence layer), NOT links/edges; if nothing backs it, omit 'quellen' or leave it empty.\n\
\n\
OUTPUT FORMAT: Return EXCLUSIVELY a single JSON object, nothing else (no prose text, no code fences before/after), in exactly this form:\n\
{\n\
  \"ops\": [\n\
    { \"action\": \"add\", \"typ\": \"observation|fact|insight|decision|research|hypothesis\",\n\
      \"inhalt\": \"…\", \"begruendung\": \"…\", \"tags\": [\"…\"], \"quellen\": [\"<exact document id>\"],\n\
      \"links\": [ { \"ziel\": \"<id|unique title substring>\", \"polaritaet\": \"supports|contradicts|replaces\", \"als\": \"parent|child\" } ] },\n\
    { \"action\": \"update\", \"id\": \"…\", \"fields\": { \"inhalt\": \"…\", \"ueberholt\": true, \"quellen\": [\"<exact document id>\"] } },\n\
    { \"action\": \"link\", \"von\": \"<id|title>\", \"zu\": \"<id|title>\", \"polaritaet\": \"supports|contradicts|replaces\" },\n\
    { \"action\": \"noop\" }\n\
  ],\n\
  \"rolling_summary\": \"compact, updated summary of the session arc (in the conversation's language)\",\n\
  \"hint\": \"\"\n\
}\n\
The JSON MAY additionally contain an optional top-level 'hint' field: if it looks like the agent needs existing project-wide knowledge it has not fetched, put a SINGLE-LINE pointer in 'hint' (not a command, not a task — just 'knowledge about X exists'); otherwise leave it empty.\n\
If there is nothing durable to capture, return an empty 'ops' list (or a single noop) and only update 'rolling_summary'.";

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- parse_checked: failure must be reportable (B1) ----------------

    #[test]
    fn malformed_json_is_an_error_not_an_empty_result() {
        // The live 2026-08-04 failure: a German opening quote „ closed with a straight
        // ASCII quote terminates the JSON string early and cost a whole extraction.
        let broken = "{\"ops\":[{\"action\":\"add\",\"typ\":\"decision\",\
            \"inhalt\":\"Feature \u{201e}Session verschieben\" wird geplant\",\
            \"begruendung\":\"x\"}],\"rolling_summary\":\"s\"}";
        // NOTE: the mismatched quote flips string parity, so the balanced-object scanner
        // finds nothing — the widest-span fallback is what turns this into a *named*
        // syntax error rather than a useless "no JSON".
        let got = parse_checked(broken);
        assert!(
            matches!(&got, Err(ParseError::Invalid(msg)) if msg.contains("expected")),
            "malformed JSON must surface as a NAMED syntax error, got {got:?}"
        );
        // The tolerant wrapper still collapses it — which is precisely why the
        // extraction path must not use it.
        assert!(parse(broken).ops.is_empty());
    }

    #[test]
    fn prose_without_json_is_reported_as_no_json() {
        // A drifted agent answering the conversation instead of extracting.
        let prose = "Das ist ein Bug in der Move-Implementierung. Implementiere ich eben.";
        assert!(matches!(parse_checked(prose), Err(ParseError::NoJson)));
    }

    #[test]
    fn valid_json_with_zero_ops_stays_a_success() {
        // The legitimate "nothing durable here" case must NOT look like a failure.
        let r = parse_checked(r#"{"ops":[],"rolling_summary":"nichts Neues"}"#)
            .expect("empty-but-valid must parse");
        assert!(r.ops.is_empty());
        assert_eq!(r.rolling_summary, "nichts Neues");
    }

    #[test]
    fn per_op_tolerance_is_unchanged_within_valid_json() {
        // Unchanged by B1: an unknown `action` degrades to Noop (`#[serde(other)]`) and
        // an op with missing required fields is dropped — neither fails the pass.
        let s = r#"{"ops":[
            {"action":"add","typ":"fact","inhalt":"gilt","begruendung":"weil"},
            {"action":"nonsense"},
            {"action":"add","typ":"fact"}
        ],"rolling_summary":"x"}"#;
        let r = parse_checked(s).expect("valid outer JSON");
        assert_eq!(r.ops.len(), 2, "add + noop kept, incomplete add dropped");
        assert!(matches!(r.ops[1], Op::Noop), "unknown action degrades to Noop");
    }

    #[test]
    fn fenced_json_still_parses() {
        let s = "```json\n{\"ops\":[],\"rolling_summary\":\"f\"}\n```";
        assert_eq!(parse_checked(s).expect("fenced").rolling_summary, "f");
    }

    #[test]
    fn parse_add_with_links() {
        let s = r#"{
          "ops": [
            { "action": "add", "typ": "decision", "inhalt": "Use WAL mode",
              "begruendung": "for concurrency", "tags": ["db"],
              "links": [ { "ziel": "SQLite chosen", "polaritaet": "supports", "als": "parent" } ] }
          ],
          "rolling_summary": "set up the DB"
        }"#;
        let r = parse(s);
        assert_eq!(r.rolling_summary, "set up the DB");
        assert_eq!(r.ops.len(), 1);
        match &r.ops[0] {
            Op::Add {
                typ,
                inhalt,
                begruendung,
                tags,
                links,
                ..
            } => {
                assert_eq!(typ, "decision");
                assert_eq!(inhalt, "Use WAL mode");
                assert_eq!(begruendung, "for concurrency");
                assert_eq!(tags, &vec!["db".to_string()]);
                assert_eq!(links.len(), 1);
                assert_eq!(links[0].ziel, "SQLite chosen");
                assert_eq!(links[0].polaritaet, "supports");
                assert_eq!(links[0].als, "parent");
            }
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn parse_add_link_defaults() {
        // links may omit polaritaet/als; tags/links may be absent entirely.
        let s = r#"{ "ops": [
            { "action": "add", "typ": "fact", "inhalt": "X", "begruendung": "user said",
              "links": [ { "ziel": "node-1" } ] }
        ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Add { links, tags, .. } => {
                assert!(tags.is_empty());
                assert_eq!(links[0].polaritaet, "supports");
                assert_eq!(links[0].als, "parent");
            }
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn parse_update_partial_fields() {
        let s = r#"{ "ops": [
            { "action": "update", "id": "abc", "fields": { "inhalt": "new text", "ueberholt": true } }
        ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Update { id, fields } => {
                assert_eq!(id, "abc");
                assert_eq!(fields.inhalt.as_deref(), Some("new text"));
                assert_eq!(fields.ueberholt, Some(true));
                // unset fields stay None
                assert_eq!(fields.begruendung, None);
                assert_eq!(fields.typ, None);
                assert_eq!(fields.tags, None);
                assert_eq!(fields.erledigt, None);
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn parse_update_no_fields() {
        // fields may be omitted entirely → default (all None).
        let s = r#"{ "ops": [ { "action": "update", "id": "abc" } ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Update { id, fields } => {
                assert_eq!(id, "abc");
                assert_eq!(fields, &UpdateFields::default());
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn parse_link() {
        let s = r#"{ "ops": [
            { "action": "link", "von": "A", "zu": "B", "polaritaet": "contradicts" }
        ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Link { von, zu, polaritaet } => {
                assert_eq!(von, "A");
                assert_eq!(zu, "B");
                assert_eq!(polaritaet, "contradicts");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn parse_noop() {
        let s = r#"{ "ops": [ { "action": "noop" } ] }"#;
        let r = parse(s);
        assert_eq!(r.ops, vec![Op::Noop]);
    }

    #[test]
    fn parse_chatty_prose_around_json() {
        let s = "Sure! Here is what I extracted:\n\n\
                 { \"ops\": [ { \"action\": \"noop\" } ], \"rolling_summary\": \"nothing durable\" }\n\n\
                 Let me know if you need anything else.";
        let r = parse(s);
        assert_eq!(r.ops, vec![Op::Noop]);
        assert_eq!(r.rolling_summary, "nothing durable");
    }

    #[test]
    fn parse_json_code_fence() {
        let s = "```json\n{ \"ops\": [ { \"action\": \"add\", \"typ\": \"fact\", \
                 \"inhalt\": \"Z\", \"begruendung\": \"src\" } ], \"rolling_summary\": \"s\" }\n```";
        let r = parse(s);
        assert_eq!(r.ops.len(), 1);
        assert_eq!(r.rolling_summary, "s");
        match &r.ops[0] {
            Op::Add { inhalt, .. } => assert_eq!(inhalt, "Z"),
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn parse_braces_inside_strings() {
        // A `}` inside a string value must not end the object early.
        let s = r#"prefix { "ops": [], "rolling_summary": "uses a closing brace } in text" } suffix"#;
        let r = parse(s);
        assert!(r.ops.is_empty());
        assert_eq!(r.rolling_summary, "uses a closing brace } in text");
    }

    #[test]
    fn parse_garbage_returns_default() {
        assert_eq!(parse("this is not json at all"), ExtractionResult::default());
        assert_eq!(parse(""), ExtractionResult::default());
        // malformed object → default, no panic
        assert_eq!(parse("{ not valid json"), ExtractionResult::default());
    }

    #[test]
    fn parse_unknown_action_becomes_noop() {
        let s = r#"{ "ops": [ { "action": "frobnicate", "whatever": 1 } ] }"#;
        let r = parse(s);
        assert_eq!(r.ops, vec![Op::Noop]);
    }

    #[test]
    fn parse_one_bad_op_does_not_poison_array() {
        // First op is malformed (missing required `inhalt`/`begruendung`),
        // it is dropped; the valid noop survives.
        let s = r#"{ "ops": [
            { "action": "add", "typ": "fact" },
            { "action": "noop" }
        ], "rolling_summary": "kept" }"#;
        let r = parse(s);
        assert_eq!(r.ops, vec![Op::Noop]);
        assert_eq!(r.rolling_summary, "kept");
    }

    #[test]
    fn parse_rolling_summary_round_trips() {
        let s = r#"{ "ops": [], "rolling_summary": "the session set up DB + terminal" }"#;
        let r = parse(s);
        assert!(r.ops.is_empty());
        assert_eq!(r.rolling_summary, "the session set up DB + terminal");
    }

    #[test]
    fn extraction_prompt_is_english_and_specifies_json() {
        assert!(EXTRACTION_PROMPT.contains("You are observing"));
        assert!(EXTRACTION_PROMPT.contains("rolling_summary"));
        assert!(EXTRACTION_PROMPT.contains("\"action\": \"add\""));
        // English schema codes appear in the JSON contract.
        assert!(EXTRACTION_PROMPT.contains("observation|fact|insight|decision|research|hypothesis"));
        assert!(EXTRACTION_PROMPT.contains("supports|contradicts|replaces"));
    }

    #[test]
    fn extraction_prompt_carries_source_language_rule() {
        // The CRITICAL rule: content language follows the conversation, NOT this prompt.
        assert!(EXTRACTION_PROMPT.contains("SAME language as the conversation"));
        assert!(EXTRACTION_PROMPT.contains("Do NOT translate the content to English"));
        assert!(EXTRACTION_PROMPT.contains("must NOT change the language of what you record"));
    }

    #[test]
    fn extraction_prompt_mentions_optional_hint() {
        assert!(EXTRACTION_PROMPT.contains("'hint'"));
        assert!(EXTRACTION_PROMPT.contains("SINGLE-LINE pointer"));
    }

    #[test]
    fn parse_hint_when_present() {
        let s = r#"{ "ops": [], "rolling_summary": "s",
            "hint": "knowledge about pathfinding already exists" }"#;
        let r = parse(s);
        assert_eq!(r.hint, "knowledge about pathfinding already exists");
        assert_eq!(r.rolling_summary, "s");
    }

    #[test]
    fn parse_add_with_quellen() {
        let s = r#"{ "ops": [
            { "action": "add", "typ": "fact", "inhalt": "X", "begruendung": "doc'd in F004",
              "quellen": [".claude/features/F004-weapons.md", ".claude/adr/003.md"] }
        ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Add { quellen, .. } => {
                assert_eq!(
                    quellen,
                    &vec![
                        ".claude/features/F004-weapons.md".to_string(),
                        ".claude/adr/003.md".to_string()
                    ]
                );
            }
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn parse_add_quellen_absent_is_empty() {
        let s = r#"{ "ops": [
            { "action": "add", "typ": "fact", "inhalt": "X", "begruendung": "src" }
        ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Add { quellen, .. } => assert!(quellen.is_empty()),
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn parse_update_fields_quellen() {
        let s = r#"{ "ops": [
            { "action": "update", "id": "abc",
              "fields": { "quellen": [".claude/features/observer.md"] } }
        ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Update { fields, .. } => {
                assert_eq!(
                    fields.quellen.as_deref(),
                    Some(&[".claude/features/observer.md".to_string()][..])
                );
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn extraction_prompt_mentions_documents_and_quellen() {
        assert!(EXTRACTION_PROMPT.contains("Available project documents"));
        assert!(EXTRACTION_PROMPT.contains("quellen"));
        assert!(EXTRACTION_PROMPT.contains("NEVER invented"));
    }

    #[test]
    fn parse_hint_absent_defaults_empty() {
        let s = r#"{ "ops": [ { "action": "noop" } ], "rolling_summary": "s" }"#;
        let r = parse(s);
        assert_eq!(r.hint, "");
    }
}

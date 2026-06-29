//! Observer extraction contract (JSON, Observer → Rust) + the extraction prompt.
//! Pure serde, no cross-module deps.
//!
//! The headless observer LLM returns a JSON object of the shape:
//! ```json
//! {
//!   "ops": [
//!     { "action": "add", "typ": "beobachtung|fakt|erkenntnis|entscheidung|recherche|vermutung",
//!       "inhalt": "…", "begruendung": "…", "tags": ["…"], "quellen": ["<doc-id>"],
//!       "links": [ { "ziel": "<id|title>", "polaritaet": "stuetzt|widerspricht|ersetzt",
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
    "stuetzt".to_string()
}

fn default_als() -> String {
    "parent".to_string()
}

/// An inline link spec attached to an `add` op.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LinkSpec {
    /// Target node — an id OR a unique title substring (resolved by the engine).
    pub ziel: String,
    /// "stuetzt" | "widerspricht" | "ersetzt" (default "stuetzt").
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
        /// an EXACT id from the "Verfügbare Projekt-Dokumente" input list (Rust
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

/// Parse the observer's stdout into an [`ExtractionResult`].
///
/// Robust against chatter and one malformed op: returns `ExtractionResult::default()`
/// on any unrecoverable failure (no JSON / undeserializable outer object). NEVER panics.
pub fn parse(stdout: &str) -> ExtractionResult {
    let unfenced = strip_code_fences(stdout);
    let json = match extract_json_object(&unfenced) {
        Some(j) => j,
        None => return ExtractionResult::default(),
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

    let raw: Raw = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(_) => return ExtractionResult::default(),
    };

    let ops = raw
        .ops
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Op>(v).ok())
        .collect();

    ExtractionResult {
        ops,
        rolling_summary: raw.rolling_summary,
        hint: raw.hint,
    }
}

/// The extraction prompt handed to the headless observer LLM (Slice A, Schritt 7).
///
/// 3rd-person observer variant of the knowledge rules. German, to match the
/// existing knowledge rules. Per-project overridable later (Slice B / tuning).
pub const EXTRACTION_PROMPT: &str = "Du beobachtest einen Dialog zwischen einem User und einem Coding-Agent. Deine Aufgabe ist es, daraus **dauerhaftes, projektweites** Wissen zu extrahieren und in strukturierte Operationen (ADD/UPDATE/LINK) auf einen bewerteten Evidenz-DAG zu übersetzen. Du selbst gibst keine Antwort an den User und steuerst den Agent nicht — du pflegst nur das Wissen.\n\
\n\
REGELN:\n\
(1) Erfasse NUR dauerhaftes, projektweites Wissen. Speichere KEINE transienten, session-lokalen Prozess-Ereignisse oder Steuerungen — z. B. 'dieser Schritt macht nur X', 'Y kommt später', 'ein anderer Agent macht Z', 'User sagt los', 'Tuning übersprungen', aktuelles Task-Scoping. Solches gehört in den Transcript, nicht in den Graph. Enthält eine Nachricht zusätzlich einen dauerhaften Fakt oder eine Entscheidung (z. B. ein Datenformat), erfasse NUR diesen Teil.\n\
(2) Wähle den Knoten-Typ nach QUELLE und SICHERHEIT (nicht nach Thema), abgeleitet aus der Formulierung: fakt = sichere harte Info ('es ist X', vom User oder aus einer Datenbank), beobachtung = direkt beobachtet / 'sieht aus wie', recherche = extern nachgeschlagen, vermutung = unsicher/abgeschwächt ('ich denke', 'vermutlich'), erkenntnis = eine selbst hergeleitete Schlussfolgerung, entscheidung = eine getroffene Wahl. Der User ist NICHT allwissend — urteile nach der Formulierung, nicht danach, dass es gesagt wurde. Bist du unsicher, wie sicher etwas ist oder woher es kommt, extrahiere es NICHT (im Zweifel weglassen statt raten).\n\
(3) Jede ADD-Operation BENÖTIGT eine 'begruendung' (sonst wird die Op verworfen), typ-adaptiv: für beobachtung/recherche/vermutung die Frage, die sie beantwortet (als Frage); für erkenntnis die Begründung/Herleitung; für entscheidung ihr Zweck ('damit'/wofür sie da ist); für fakt die Quelle, aus der sie stammt.\n\
(4) Baue einen GRAPH, keine flache Liste: wenn mehrere beobachtung/recherche/vermutung auf eine übergeordnete Erkenntnis zeigen, lege eine erkenntnis an und verbinde die Evidenz (polaritaet 'stuetzt', oder 'widerspricht' für Gegen-Evidenz); stütze eine entscheidung auf das, was sie trägt. Lege diese Kanten INLINE über das 'links'-Feld der ADD-Op an (ziel = id ODER eindeutiger Titel-Substring, polaritaet, als 'parent'|'child').\n\
(5) Entscheidungen sind ein Lifecycle, kein Evidenz-Votum: Status ist aktiv / erledigt / überholt, und eine Entscheidung ist nicht 'schwach', nur weil ihr Evidenz fehlt. Ändert sich eine Wahl, verknüpfe die NEUE Entscheidung mit der alten über eine LINK-Op mit polaritaet='ersetzt' (alte → überholt) — lass NIE widersprüchliche entscheidung-Knoten nebeneinander stehen. Ist eine Entscheidung schlicht UMGESETZT (ein erledigter Schritt), markiere sie via UPDATE mit fields.erledigt=true. Korrigiere oder ziehe falsche/veraltete Knoten via UPDATE mit fields.ueberholt=true zurück, statt Duplikate anzuhäufen.\n\
(6) DEDUP-Bewusstsein: Dir wird ein Ausschnitt des bestehenden DAG (Knoten mit id/Titel/Tags) mitgeliefert. Prüfe ihn, BEVOR du etwas Neues anlegst. Existiert der Gedanke bereits (auch anders formuliert), nutze UPDATE (Knoten verfeinern) oder LINK (verbinden) statt eines blinden ADD. Erweitere frühere erkenntnis-Knoten, wenn neue Evidenz eintrifft, statt neue anzulegen.\n\
(7) Versieh jeden Knoten über 'tags' mit seinem Thema/Bereich (Subsystem oder Work-Stream, z. B. 'pathfinding', 'voxel'), damit spätere Arbeit gezielt nur diesen Strom abrufen kann.\n\
(8) Halte jeden Knoten auf EINE knappe Aussage. Erfasse auch einmalige/datierte Spezifika (der Zeitstempel wird automatisch gesetzt). Reine Höflichkeiten überspringst du.\n\
(9) DOKUMENTE ALS QUELLEN: Der Eingabe ist ein Abschnitt 'Verfügbare Projekt-Dokumente' vorangestellt, der jedes Dokument mit seiner exakten 'id' und seinem Titel auflistet. Ist ein extrahiertes Wissensstück durch eines dieser Dokumente belegt / daraus abgeleitet / darin dokumentiert (z. B. ein 'fakt', dessen Quelle ein Feature-Doc ist, oder eine 'entscheidung', die in F00x dokumentiert ist), zitiere die relevante(n) Dokument-'id(s)' im 'quellen'-Array der Op — mit der EXAKTEN id aus der Liste, NIEMALS erfunden. 'quellen' sind QUELLEN (eine orthogonale Beleg-Schicht), KEINE Links/Kanten; ist nichts belegt, lass 'quellen' weg oder leer.\n\
\n\
AUSGABE-FORMAT: Gib AUSSCHLIESSLICH ein einzelnes JSON-Objekt zurück, nichts sonst (kein Prosa-Text, keine Code-Fences davor/danach), in genau dieser Form:\n\
{\n\
  \"ops\": [\n\
    { \"action\": \"add\", \"typ\": \"beobachtung|fakt|erkenntnis|entscheidung|recherche|vermutung\",\n\
      \"inhalt\": \"…\", \"begruendung\": \"…\", \"tags\": [\"…\"], \"quellen\": [\"<exakte Dokument-id>\"],\n\
      \"links\": [ { \"ziel\": \"<id|eindeutiger Titel-Substring>\", \"polaritaet\": \"stuetzt|widerspricht|ersetzt\", \"als\": \"parent|child\" } ] },\n\
    { \"action\": \"update\", \"id\": \"…\", \"fields\": { \"inhalt\": \"…\", \"ueberholt\": true, \"quellen\": [\"<exakte Dokument-id>\"] } },\n\
    { \"action\": \"link\", \"von\": \"<id|Titel>\", \"zu\": \"<id|Titel>\", \"polaritaet\": \"stuetzt|widerspricht|ersetzt\" },\n\
    { \"action\": \"noop\" }\n\
  ],\n\
  \"rolling_summary\": \"kompakte, aktualisierte Zusammenfassung des Session-Bogens\",\n\
  \"hint\": \"\"\n\
}\n\
Das JSON DARF zusätzlich ein optionales Top-Level-Feld 'hint' enthalten: Wenn es wirkt, als bräuchte der Agent vorhandenes projektweites Wissen, das er nicht geholt hat, gib in 'hint' einen EINZEILIGEN Pointer (kein Befehl, keine Aufgabe — nur 'Zu X existiert Wissen'); sonst leer.\n\
Gibt es nichts Dauerhaftes zu erfassen, gib eine leere 'ops'-Liste (oder eine einzelne noop) zurück und aktualisiere nur 'rolling_summary'.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add_with_links() {
        let s = r#"{
          "ops": [
            { "action": "add", "typ": "entscheidung", "inhalt": "Use WAL mode",
              "begruendung": "for concurrency", "tags": ["db"],
              "links": [ { "ziel": "SQLite chosen", "polaritaet": "stuetzt", "als": "parent" } ] }
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
                assert_eq!(typ, "entscheidung");
                assert_eq!(inhalt, "Use WAL mode");
                assert_eq!(begruendung, "for concurrency");
                assert_eq!(tags, &vec!["db".to_string()]);
                assert_eq!(links.len(), 1);
                assert_eq!(links[0].ziel, "SQLite chosen");
                assert_eq!(links[0].polaritaet, "stuetzt");
                assert_eq!(links[0].als, "parent");
            }
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn parse_add_link_defaults() {
        // links may omit polaritaet/als; tags/links may be absent entirely.
        let s = r#"{ "ops": [
            { "action": "add", "typ": "fakt", "inhalt": "X", "begruendung": "user said",
              "links": [ { "ziel": "node-1" } ] }
        ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Add { links, tags, .. } => {
                assert!(tags.is_empty());
                assert_eq!(links[0].polaritaet, "stuetzt");
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
            { "action": "link", "von": "A", "zu": "B", "polaritaet": "widerspricht" }
        ] }"#;
        let r = parse(s);
        match &r.ops[0] {
            Op::Link { von, zu, polaritaet } => {
                assert_eq!(von, "A");
                assert_eq!(zu, "B");
                assert_eq!(polaritaet, "widerspricht");
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
        let s = "```json\n{ \"ops\": [ { \"action\": \"add\", \"typ\": \"fakt\", \
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
            { "action": "add", "typ": "fakt" },
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
    fn extraction_prompt_is_german_and_specifies_json() {
        assert!(EXTRACTION_PROMPT.contains("Du beobachtest"));
        assert!(EXTRACTION_PROMPT.contains("rolling_summary"));
        assert!(EXTRACTION_PROMPT.contains("\"action\": \"add\""));
    }

    #[test]
    fn extraction_prompt_mentions_optional_hint() {
        assert!(EXTRACTION_PROMPT.contains("'hint'"));
        assert!(EXTRACTION_PROMPT.contains("EINZEILIGEN Pointer"));
    }

    #[test]
    fn parse_hint_when_present() {
        let s = r#"{ "ops": [], "rolling_summary": "s",
            "hint": "Zu pathfinding existiert bereits Wissen" }"#;
        let r = parse(s);
        assert_eq!(r.hint, "Zu pathfinding existiert bereits Wissen");
        assert_eq!(r.rolling_summary, "s");
    }

    #[test]
    fn parse_add_with_quellen() {
        let s = r#"{ "ops": [
            { "action": "add", "typ": "fakt", "inhalt": "X", "begruendung": "doc'd in F004",
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
            { "action": "add", "typ": "fakt", "inhalt": "X", "begruendung": "src" }
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
        assert!(EXTRACTION_PROMPT.contains("Verfügbare Projekt-Dokumente"));
        assert!(EXTRACTION_PROMPT.contains("quellen"));
        assert!(EXTRACTION_PROMPT.contains("NIEMALS erfunden"));
    }

    #[test]
    fn parse_hint_absent_defaults_empty() {
        let s = r#"{ "ops": [ { "action": "noop" } ], "rolling_summary": "s" }"#;
        let r = parse(s);
        assert_eq!(r.hint, "");
    }
}

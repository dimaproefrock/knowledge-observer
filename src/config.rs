//! Layered configuration for the observer plugin.
//!
//! Resolution priority per key (highest wins), so the plugin can run in many
//! projects at once with **individual storage locations per project**:
//!
//! ```text
//! <project>/.claude/observer.json   (per-project file — HIGHEST)
//!   > env  (CLAUDE_PLUGIN_OPTION_<KEY>  from native plugin userConfig, or OBSERVER_<KEY>)
//!   > built-in defaults
//! ```
//!
//! `knowledge_dir` default is **relative** (`.claude/knowledge`) → each project
//! resolves it against its own root, so multiple projects stay isolated without any
//! configuration. An **absolute** `knowledge_dir` is only safe **per project** (in the
//! file); set globally (env), it would funnel every project into one folder — we warn.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub enabled: bool,
    /// Relative (per-project isolation) or absolute (per-project override only).
    pub knowledge_dir: String,
    pub model: Option<String>,
    pub idle_daemon_secs: u64,
    /// Minimum seconds between observer-agent runs for one session. Rapid Stop-hook
    /// triggers within this window are coalesced into the next pass after the cooldown.
    pub min_interval_secs: u64,
    pub obs_max_turns: u32,
    pub max_decisions: usize,
    pub max_questions: usize,
    /// Appended to the built-in extraction prompt (safe customization).
    pub prompt_extra: Option<String>,
    /// Full prompt override (advanced; must keep the JSON ops contract).
    pub prompt_file: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            knowledge_dir: ".claude/knowledge".to_string(),
            model: None,
            idle_daemon_secs: 600,
            min_interval_secs: 45,
            obs_max_turns: 30,
            max_decisions: 15,
            max_questions: 15,
            prompt_extra: None,
            prompt_file: None,
        }
    }
}

/// The per-project file shape — every field optional (it only overrides).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PartialConfig {
    enabled: Option<bool>,
    knowledge_dir: Option<String>,
    model: Option<String>,
    idle_daemon_secs: Option<u64>,
    min_interval_secs: Option<u64>,
    obs_max_turns: Option<u32>,
    max_decisions: Option<usize>,
    max_questions: Option<usize>,
    prompt_extra: Option<String>,
    prompt_file: Option<String>,
}

impl Config {
    /// Resolve config for a project from the real environment + per-project file.
    pub fn resolve(project_dir: &Path) -> Config {
        let file = std::fs::read_to_string(project_dir.join(".claude").join("observer.json")).ok();
        resolve_layered(&real_env, file.as_deref())
    }

    /// Absolute knowledge dir for this project (relative paths resolve against the root).
    pub fn knowledge_dir_abs(&self, project_dir: &Path) -> PathBuf {
        let p = Path::new(&self.knowledge_dir);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            project_dir.join(p)
        }
    }
}

/// Read a config value from the environment. `resolve_layered` passes the key in
/// UPPER_SNAKE; native plugin `userConfig` injects the **literal lowercase** field
/// name as `CLAUDE_PLUGIN_OPTION_<key>`, so we try the lowercase form first, then an
/// uppercase `CLAUDE_PLUGIN_OPTION_<KEY>` (defensive), then a plain `OBSERVER_<KEY>`.
fn real_env(key: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    let upper = key.to_ascii_uppercase();
    for name in [
        format!("CLAUDE_PLUGIN_OPTION_{lower}"),
        format!("CLAUDE_PLUGIN_OPTION_{upper}"),
        format!("OBSERVER_{upper}"),
    ] {
        if let Ok(v) = std::env::var(&name) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Pure, testable resolver: defaults < env < per-project file.
fn resolve_layered(get_env: &dyn Fn(&str) -> Option<String>, file_json: Option<&str>) -> Config {
    let mut cfg = Config::default();

    // --- env layer (middle priority) ---
    if let Some(v) = get_env("ENABLED").and_then(|s| parse_bool(&s)) {
        cfg.enabled = v;
    }
    let mut knowledge_dir_from_file = false;
    if let Some(v) = get_env("KNOWLEDGE_DIR") {
        cfg.knowledge_dir = v;
    }
    if let Some(v) = get_env("MODEL") {
        cfg.model = Some(v);
    }
    if let Some(v) = get_env("IDLE_DAEMON_SECS").and_then(|s| s.parse().ok()) {
        cfg.idle_daemon_secs = v;
    }
    if let Some(v) = get_env("MIN_INTERVAL_SECS").and_then(|s| s.parse().ok()) {
        cfg.min_interval_secs = v;
    }
    if let Some(v) = get_env("OBS_MAX_TURNS").and_then(|s| s.parse().ok()) {
        cfg.obs_max_turns = v;
    }
    if let Some(v) = get_env("MAX_DECISIONS").and_then(|s| s.parse().ok()) {
        cfg.max_decisions = v;
    }
    if let Some(v) = get_env("MAX_QUESTIONS").and_then(|s| s.parse().ok()) {
        cfg.max_questions = v;
    }
    if let Some(v) = get_env("PROMPT_EXTRA") {
        cfg.prompt_extra = Some(v);
    }
    if let Some(v) = get_env("PROMPT_FILE") {
        cfg.prompt_file = Some(v);
    }

    // --- per-project file layer (highest priority) ---
    if let Some(json) = file_json {
        if let Ok(p) = serde_json::from_str::<PartialConfig>(json) {
            if let Some(v) = p.enabled {
                cfg.enabled = v;
            }
            if let Some(v) = p.knowledge_dir {
                cfg.knowledge_dir = v;
                knowledge_dir_from_file = true;
            }
            if let Some(v) = p.model {
                cfg.model = Some(v);
            }
            if let Some(v) = p.idle_daemon_secs {
                cfg.idle_daemon_secs = v;
            }
            if let Some(v) = p.min_interval_secs {
                cfg.min_interval_secs = v;
            }
            if let Some(v) = p.obs_max_turns {
                cfg.obs_max_turns = v;
            }
            if let Some(v) = p.max_decisions {
                cfg.max_decisions = v;
            }
            if let Some(v) = p.max_questions {
                cfg.max_questions = v;
            }
            if let Some(v) = p.prompt_extra {
                cfg.prompt_extra = Some(v);
            }
            if let Some(v) = p.prompt_file {
                cfg.prompt_file = Some(v);
            }
        }
    }

    // --- foot-gun guard: an ABSOLUTE knowledge_dir from a GLOBAL source (env) would
    // funnel every project into one folder. Only a per-project file should set absolute.
    if Path::new(&cfg.knowledge_dir).is_absolute() && !knowledge_dir_from_file {
        eprintln!(
            "[observer] WARNING: absolute knowledge_dir '{}' came from a global source (env/userConfig). \
             Set absolute paths per project in <project>/.claude/observer.json, or use a relative path \
             so each project keeps its own knowledge.",
            cfg.knowledge_dir
        );
    }

    cfg
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_when_nothing_set() {
        let cfg = resolve_layered(&no_env, None);
        assert_eq!(cfg, Config::default());
        assert!(!Path::new(&cfg.knowledge_dir).is_absolute(), "default is relative");
    }

    #[test]
    fn env_overrides_defaults() {
        let env = |k: &str| match k {
            "KNOWLEDGE_DIR" => Some(".claude/knowledge".to_string()),
            "IDLE_DAEMON_SECS" => Some("1200".to_string()),
            "ENABLED" => Some("false".to_string()),
            _ => None,
        };
        let cfg = resolve_layered(&env, None);
        assert_eq!(cfg.knowledge_dir, ".claude/knowledge");
        assert_eq!(cfg.idle_daemon_secs, 1200);
        assert!(!cfg.enabled);
    }

    #[test]
    fn project_file_wins_over_env() {
        let env = |k: &str| match k {
            "KNOWLEDGE_DIR" => Some(".claude/knowledge".to_string()),
            "MODEL" => Some("from-env".to_string()),
            _ => None,
        };
        let file = r#"{ "knowledge_dir": "D:/wissen/projA", "max_decisions": 5 }"#;
        let cfg = resolve_layered(&env, Some(file));
        // Per-project file wins for knowledge_dir.
        assert_eq!(cfg.knowledge_dir, "D:/wissen/projA");
        // Env still applies where the file is silent.
        assert_eq!(cfg.model.as_deref(), Some("from-env"));
        assert_eq!(cfg.max_decisions, 5);
    }

    #[test]
    fn relative_knowledge_dir_resolves_per_project() {
        let cfg = Config::default();
        let abs = cfg.knowledge_dir_abs(Path::new("/projects/alpha"));
        assert!(abs.ends_with("knowledge"));
        assert!(abs.starts_with("/projects/alpha") || abs.to_string_lossy().contains("alpha"));
    }

    #[test]
    fn absolute_knowledge_dir_used_as_is() {
        let mut cfg = Config::default();
        cfg.knowledge_dir = "/var/knowledge".to_string();
        let abs = cfg.knowledge_dir_abs(Path::new("/projects/alpha"));
        assert_eq!(abs, Path::new("/var/knowledge"));
    }

    #[test]
    fn malformed_file_is_ignored() {
        let cfg = resolve_layered(&no_env, Some("{ not json"));
        assert_eq!(cfg, Config::default());
    }
}

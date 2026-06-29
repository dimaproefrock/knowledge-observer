# Knowledge Observer — a Claude Code plugin

A transparent **knowledge observer** for Claude Code. It watches your work in the
background and builds a human-readable knowledge graph of your project's
**decisions**, **facts**, and **open questions** — then feeds that knowledge back
into every session so Claude stays oriented over time. Host-independent and
standalone: runs as a plain Claude Code plugin on your subscription.

## What it does

Three hooks wire the observer into Claude Code's session lifecycle:

- **`SessionStart`** → `observer knowledge-index`
  Injects the current project knowledge (active decisions + open questions) as
  `additionalContext` when a session starts/resumes/clears/compacts. Reads the store
  **directly** (daemon-independent) — robust even before the daemon runs.
- **`UserPromptSubmit`** → `observer knowledge-hint`
  Delivers any pending relevance hint for this session as `additionalContext`.
- **`Stop`** (async) → `observer observer-trigger`
  When a turn finishes, lazily autostarts a per-project **single-writer daemon** and
  fires a fire-and-forget trigger. The daemon tails the new transcript turns and runs a
  persistent per-session **observer agent** (`claude --resume`, your subscription) that
  extracts knowledge into the graph in the background. Being `async`, it never blocks you.

Injected context lands in the model's context **only** — it is not persisted to the
JSONL transcript, so there is no feedback loop.

## How it works

- A **lazy-started daemon** (one per project) is the single writer of the store. It is
  spawned on demand by the `Stop` hook and idle-shuts-down on its own.
- The store is a **human-readable, git-diffable** `.md`-per-node graph (one Markdown file
  per node + `edges.json`), created under the configured `knowledge_dir`.
- The observer LLM is a **persistent per-session agent** resumed via `claude --resume` — so
  it remembers what it has recorded and dedupes semantically on its own. **Subscription-based**
  (Claude Pro/Max), **no API key**. It runs least-privilege (no `--dangerously-skip-permissions`)
  and with `--setting-sources user`, so it never triggers the project's own hooks.

## Install

This plugin bundles a native `observer` executable — build and place it first
(see [`bin/README.md`](./bin/README.md)):

```sh
cargo build --release --bin observer
cp target/release/observer bin/observer    # or observer.exe on Windows
```

Local development:

```sh
claude --plugin-dir ./
```

Via a marketplace (git):

```sh
/plugin marketplace add dimaproefrock/knowledge-observer
/plugin install knowledge-observer@knowledge-observer-marketplace
```

## Configuration

Settings resolve **per key, highest wins**:

```
<project>/.claude/observer.json   >   native userConfig / env (CLAUDE_PLUGIN_OPTION_* / OBSERVER_*)   >   defaults
```

- **`knowledge_dir`** — storage location. The default `.claude/knowledge` is **relative**, so
  every project keeps its own isolated graph automatically. To put a project's knowledge in a
  **custom/absolute** location, set it **per project** in `<project>/.claude/observer.json`
  (a global absolute path would merge all projects into one folder).
- **`enabled`**, **`model`**, **`prompt_extra`** (appended to the extraction prompt),
  **`idle_daemon_secs`**, and the index caps are all configurable the same way.

Example `<project>/.claude/observer.json`:

```json
{ "knowledge_dir": "D:/wissen/projectA", "prompt_extra": "Record API contracts verbatim." }
```

## Notes

- **Subscription-based** — drives `claude` under your Pro/Max subscription; no API key is used.
- The daemon is loopback-only (`127.0.0.1`) and token-authenticated; the plugin sends nothing
  off-machine itself.
- A legacy single-file `graph.json` store is **migrated once** to the `.md` layout on first read
  (the old file is renamed `graph.json.converted`).

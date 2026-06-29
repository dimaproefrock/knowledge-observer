# Releasing

The plugin ships a native binary that the launcher (`bin/observer`) downloads from
the matching GitHub Release. So the plugin **content** and the **binary** must move
together. Three places hold the version and must always agree:

| File | Field | Role |
|------|-------|------|
| `.claude-plugin/plugin.json` | `version` | What Claude Code delivers to users as an update (a pinned version → an update is delivered **only** when this changes). |
| `VERSION` | `vX.Y.Z` | Which GitHub Release tag the launcher fetches the per-OS binary from. |
| `Cargo.toml` | `version` | The crate / binary version. |

## Cut a release

```sh
./release.sh 0.1.1
```

This bumps all three in lockstep, runs `cargo build`/`cargo test`, commits
`release vX.Y.Z`, tags `vX.Y.Z`, and pushes. Pushing the tag triggers
`.github/workflows/release.yml`, which builds the per-OS binaries and attaches them
as release assets (`observer-linux-x64`, `observer-macos-arm64`,
`observer-windows-x64.exe`, …).

## How users get it

Updates are **not** automatic for a third-party marketplace by default. A user moves
to the new version with:

```
/plugin update knowledge-observer@knowledge-observer-marketplace
/reload-plugins
```

…or automatically, if they turned on auto-update for the marketplace (`/plugin` UI →
Marketplaces), or their org enabled it via managed settings.

Because `version` is **pinned** in `plugin.json`, pushing commits **without** running
`release.sh` (i.e. without bumping the version) does **not** deliver an update — which
is intentional: it keeps the delivered plugin and its binary in sync.

## Notes

- `${CLAUDE_PLUGIN_DATA}` (where the launcher caches the downloaded binary, keyed by
  `VERSION`) **persists** across updates, so a version bump triggers a fresh download
  of just the new binary.
- For local development you don't need a release: drop a freshly built
  `bin/observer.exe` (or `bin/observer-native`) next to the launcher and run
  `claude --plugin-dir ./`.

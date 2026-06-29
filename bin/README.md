# `bin/` — the native `observer` executable

The plugin's hooks call a compiled `observer` binary. It is a **build artifact**, not
committed to git (see the repo `.gitignore`). Build it and drop it here:

```sh
cargo build --release --bin observer
cp target/release/observer        bin/observer        # macOS / Linux
cp target/release/observer.exe    bin/observer.exe    # Windows
```

When the plugin is enabled, Claude Code adds this `bin/` directory to `PATH`, so the
hooks can invoke `observer` by name.

For distribution, per-OS binaries are shipped via GitHub release assets / CI rather than
committed here (planned).

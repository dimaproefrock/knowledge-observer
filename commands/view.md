---
description: Open this project's knowledge graph in a browser (read-only viewer)
---

Launch the knowledge graph viewer for the **current project**.

IMPORTANT: the viewer must be pointed at the current project explicitly — pass its
**absolute root directory** as an argument, because the project dir does not arrive
reliably through the shell environment for a slash command.

Run this command **in the background** (use a background/non-blocking shell run — it
returns immediately: it spawns a detached local web server that keeps serving and opens
the browser itself, so do **not** wait for it to finish):

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/observer" view "<ABS_PROJECT_ROOT>"
```

Substitute `<ABS_PROJECT_ROOT>` with the **absolute path of the current project's root
directory** (the real path on disk, e.g. `/home/you/myproject` or `C:\Users\you\myproject`).
If the harness substitutes `${CLAUDE_PROJECT_DIR}`, you may use that as the argument
instead — it resolves to the same project root.

The launcher resolves the project, reuses an already-running viewer for it if one is
live (no duplicate servers), otherwise starts a detached server, prints a
`http://127.0.0.1:<port>` URL, and automatically opens the default browser to the
knowledge graph (nodes by type, edges, decisions/facts/open-questions, with live
refresh). It then exits immediately — the server survives in the background.

After starting it, tell the user the knowledge viewer is opening in their browser. The
server shuts itself down automatically after ~30 minutes with no open viewer tab (an
open tab keeps it alive). If the browser does not open on its own, share the printed URL
with the user.

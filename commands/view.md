---
description: Open this project's knowledge graph in a browser (read-only viewer)
---

Launch the knowledge graph viewer for the current project.

Run this command **in the background** (use a background/non-blocking shell run — it
starts a local loopback web server that keeps serving and opens the browser itself, so
do **not** wait for it to finish):

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/observer" view
```

It prints a `http://127.0.0.1:<port>` URL and automatically opens the default browser to
the knowledge graph (nodes by type, edges, decisions/facts/open-questions, with live
refresh). After starting it, tell the user the knowledge viewer is opening in their
browser and that they can stop it later by ending that background process (Ctrl-C in its
shell). If the browser does not open on its own, share the printed URL with the user.

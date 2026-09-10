# Image specs — status: orphaned, kept deliberately

JSON generation specs for the marketing and documentation diagrams (system architecture, HLS pipeline, motion FSM, incident lifecycle, MCP workflow, security model, dashboard IA, plus hero and OG-card art).

**Nothing in this repository references them.** They were consumed by `DocsDiagrams.jsx`, which rendered eight inline SVGs on `/docs` — and that component, along with the whole public site (`LandingPage`, `DocsPage`, `SecurityPage`, `LegalPage`), moved to the standalone site at sentinel-command.com. The specs stayed behind.

They also predate the rename: several carry `"SourceBox Sentry"` in their `meta.title`.

## Why they're still here

Deleting 2.1 MB of unreferenced files is easy; deleting the only copy of the design source for diagrams the marketing site still displays is not recoverable from this repo. Whether the standalone site regenerates from these or has its own copies is not knowable from here.

**If you own the standalone site:** confirm where its diagrams come from. If it has its own copies, delete this directory — it is dead weight and its brand strings are wrong. If it regenerates from these, move them there and delete this directory anyway, so the specs live beside the thing that renders them.

Either way this directory should not exist long-term. It is kept only because the safe wrong answer (keep) is cheaper than the unsafe one (delete).

*Assessed 2026-09-09.*

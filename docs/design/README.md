# Design notes

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Milestone specs, distilled from the design workflow that preceded implementation.
Each is meant to be executable without re-deriving the research behind it — the
live-probe results, verbatim Microsoft quotes and API signatures in them cost real
effort to establish, and several correct-looking alternatives were ruled out.

Each opens with a one-line goal and a concrete exit gate, and closes with a
"Settle during Mx" list of what is still unverified. Read them in order; each
assumes the ones before it.

| Milestone | Spec | Ends when |
|---|---|---|
| M1 — Entra device-code auth | [`m1-auth.md`](m1-auth.md) | `login` works against a real account, and a `login` under a running `serve` **wins** |
| M2 — MCP over HTTP, guards, first tool | [`m2-mcp-http.md`](m2-mcp-http.md) | Claude Code connects and answers *"what are my task lists"* with real data |
| M3 — Graph client: paging, retry, `$batch` | [`m3-graph-client.md`](m3-graph-client.md) | a 3-page `nextLink` walk is followed verbatim **and origin-checked**; one retry on a 429 |
| M4 — TTL cache and the read tools | [`m4-cache-read-tools.md`](m4-cache-read-tools.md) | a repeat search inside the TTL issues **zero** Graph requests; a budget-exhausted sync converges |
| M5 — `todo_agenda` and timezone | [`m5-agenda-timezone.md`](m5-agenda-timezone.md) | *"plan my day"* in one call; the DST and year-boundary fixtures pass under any host `TZ` |
| M6 — Write tools and scope-derived registration | [`m6-write-tools.md`](m6-write-tools.md) | `tools/list` is **10 vs 5** by granted scope; guards refuse with **zero** HTTP requests |
| M7 — The container, made real | [`m7-container.md`](m7-container.md) | **on Linux** a fresh named volume is `65534:65534 0700`; `SIGTERM` exits 0 within 2 s |
| M8 — Docs and release | [`m8-docs-release.md`](m8-docs-release.md) | **a second person** reaches a first tool call in under 15 minutes from the README alone |

Three of these are not ordinary build steps:

- **M2 is the kill-risk milestone.** It is where we learn whether the HTTP-only
  decision actually reaches your clients. Nothing after it is worth building until
  Claude Code connects.
- **M7 is an audit, not greenfield.** The `Dockerfile`, `compose.yaml`, `ci.yml`
  and the `hermetic-gate` action are written and have **never been built** — the
  Docker daemon was down for all of M0. M7 runs them and records what happens.
- **M8 is mostly gap-filling.** `README.md` already leads with the least-privilege
  argument. M8 creates the four documents that shipped files already point at, and
  reconciles what M1–M7 proved wrong. It also fixes two quickstart ordering bugs
  that make its own exit gate unpassable today — see
  [`m8-docs-release.md`](m8-docs-release.md) §2.

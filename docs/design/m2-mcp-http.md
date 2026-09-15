# M2 — MCP over HTTP, guards, and the first tool

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Handoff spec. This is the **kill-risk milestone**: it is where we find out whether
the HTTP-only decision actually reaches your clients. Nothing after it is worth
building until Claude Code connects.

**Exit gate.** `claude mcp add --transport http …` connects and answers *"what are
my task lists"* with real data · Hermes Agent connects · wrong bearer → 401 +
`WWW-Authenticate` · absent `Origin` → 200, foreign `Origin` → 403, foreign `Host`
→ 403 · `GET /mcp` → 405 · 2 MiB body → 413 · `/healthz` → 200, unauthenticated,
and provably Graph-free · `panic_isolation.rs` green · **each client's negotiated
`protocolVersion` recorded in the README** · the live Graph probe suite run and
recorded in `docs/graph-probe.md`, all user content redacted (m8 §6).

**Files:** `mcp.rs`, `http.rs`, `server.rs`, `tools/{mod,schema,render}.rs`,
`todo_lists`, the `healthcheck` subcommand, and the panic contract in `main.rs`.

---

## 1. Protocol era — target legacy, and say so

MCP has exactly five revisions: `2024-11-05`, `2025-03-26`, `2025-06-18`,
`2025-11-25`, `2026-07-28`. The **current** one is `2026-07-28`.

**We target the legacy family (`2025-11-25` and below) in v1.** That is a
deliberate, defensible choice — but it must never be labelled `2026-07-28`, because
2026-07-28 is not a version bump, it is a different protocol:

| 2026-07-28 change | Consequence for us |
|---|---|
| **Removes `initialize` / `notifications/initialized`** — "There is no negotiation handshake." Version + capabilities ride in every request's `_meta` | Our whole handshake would be dead code |
| **Removes protocol-level sessions** (`Mcp-Session-Id`) and SSE resumability (`Last-Event-ID`) | We implement neither, so we are already aligned here |
| **Removes `ping` and `logging/setLevel`** | We *do* implement `ping` — correct for legacy, must be dropped for modern |
| **`server/discover` MUST be implemented** | Not written |
| **`resultType: "complete" \| "input_required"` on every result** | Not written |
| **`ttlMs` + `cacheScope` REQUIRED (not optional) on `tools/list`** and five other result types | Not written |
| **`Mcp-Method` / `Mcp-Name` headers REQUIRED**, validated against the body, `-32020` on mismatch, with the `=?base64?…?=` sentinel decoded before comparison | Not written |
| Client disconnect **is** cancellation (the opposite of legacy) | Branch would live in exactly one place |

Claude Code was empirically verified to work end-to-end against a legacy-only
server: it probes `2026-07-28`, receives a clean method-not-found, and falls back.
So legacy-only is not a compatibility gamble today — but **record what each client
actually negotiates**, because that is the only evidence for whether the second era
ever earns its keep. No authoritative published list exists.

```rust
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
```

`initialize` echoes the client's version if supported, else `LATEST`. Capabilities
declare `{"tools": {}}` with **no `listChanged`** — the tool list is frozen for the
process lifetime (§6).

> **Annotations are not a security boundary.** 2026-07-28 §Tool Safety: *"clients
> MUST consider tool annotations to be untrusted."* Set them accurately because
> they drive consent UX, but **enforce read-only/destructive semantics server-side**
> regardless.

---

## 2. `mcp.rs`

Ported from [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp) with one structural change: **`&mut self` → `&self`**
throughout. [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp) wraps everything in a single global `Mutex<McpServer>`,
which serialises every request behind the slowest Graph call — fatal here.

```rust
pub trait ToolProvider: Send + Sync {
    fn list_tools(&self) -> Value;                                  // self.tools.clone()
    fn call_tool(&self, name: &str, args: &Value) -> Result<Value, RpcError>;
}
impl<T: ToolProvider> McpServer<T> {
    pub fn handle_message(&self, raw: &str) -> Option<Value>;       // &self
}
// http.rs
let state: Arc<McpServer<ServerState>> = Arc::new(mcp);             // NO Mutex
```

`handle_message` stays **pure** — every transport and test drives it directly.
Methods: `initialize`, `notifications/initialized` (returns `None` → HTTP 202),
`tools/list`, `tools/call`, `ping`. JSON-RPC batches are supported for
pre-2025-06-18 clients. Error mapping: `-32700` malformed JSON, `-32600` invalid
request, `-32601` unknown method, `-32602` unknown tool name.

**`run_stdio` is deleted** along with the stdio transport.

---

## 3. `http.rs`

`WORKERS` (4) threads via `Builder::new().stack_size(512 * 1024)` — rustls
handshakes want real stack — over one `Arc<tiny_http::Server>`.

| Route | Behaviour |
|---|---|
| `POST /mcp` | bearer-gated, `Cache-Control: no-store` on every response |
| `GET /healthz` | **unauthenticated**, never touches Graph, never touches the token store |
| everything else | 405 / 404 |

Guards, outermost first — the ordering is the property, not a detail. An
unauthenticated request must be rejected **before** any handler work:

1. **Bearer** — `subtle::ConstantTimeEq`, `.trim()` both sides (a TTY-allocated
   `docker compose run` appends `\r`). 401 + `WWW-Authenticate: Bearer`.
2. **`Origin`** — absent → allow (every non-browser MCP client sends none;
   requiring it breaks all of them). Present and foreign → 403.
3. **`Host`** — allowlist, 403 otherwise. Origin alone does not cover DNS
   rebinding.
4. **Body** — `take(MAX + 1)` → 413. Never buffer first and check after.

> **`/healthz` must never call `Server::num_connections()`.** Its body is
> `unimplemented!()` in tiny_http 0.12 (lib.rs:374) — calling it panics the health
> probe, which is the one code path whose failure looks like a dead container.

The `healthcheck` subcommand is a raw `TcpStream` `GET /healthz` (ported from [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s `health_probe()`). Mandatory: the scratch image has no shell and no curl.

---

## 4. The panic contract — all three pieces, or `panic = "abort"` is better

`Cargo.toml` sets `panic = "unwind"`. That only pays for itself with all three:

1. **`std::panic::set_hook` installed first in `main()`** — already done in M0.
   Routes payload + `Location` through `logger::error`, never the default stderr
   writer.
2. **`catch_unwind(AssertUnwindSafe(…))` around the *whole worker loop*, not just
   the handler body.** A panic in `recv_timeout()`, in the response-write path, or
   in the guard code before the handler is entered kills the thread outright — and
   four of those leave a listening socket with **no acceptors** while the container
   still reports "running" and `/healthz` hangs. `main` supervises: a returning
   `JoinHandle` respawns a replacement, bounded to N restarts, then logs and exits 1.
   On a caught handler panic, respond JSON-RPC `-32603`, falling through to
   tiny_http's `Drop`-emitted 500 if even the response write failed. (Verified safe:
   `impl Drop for Request` writes the 500 and its `notify_when_responded` is `None`
   on the normal construction path, so there is no panic-in-panic.)
3. **Every lock absorbs poison** — `.lock().unwrap_or_else(|e| e.into_inner())`. A
   caught panic *does* poison; this was measured, not assumed.

> **The cache is the one exception** and it arrives in M4: `cache: RwLock<Cache>`
> must treat poison as **reset**, not absorb. A panic mid-write can leave
> `total_tasks` incremented but `tasks` unmodified, and eviction then underflows.
> Use `clear_poison()` (stable since 1.77) and log a warning.

`tests/panic_isolation.rs` pins all of it: handler panic → HTTP 500, worker
survives, next request on the same worker succeeds.

---

## 5. Concurrency and lock order

`ServerState` fields: `agent: ureq::Agent` (Send + Sync + Clone, one pooled
connection set), `tokens: TokenProvider`, `cache: RwLock<Cache>` (M4),
`refill: Mutex<()>` (M4), `gate: Semaphore`, `tools: Value` (frozen), `cfg: Config`.

**Lock order is fixed. Violating it is a deadlock, not a style issue:**

```
refill: Mutex<()>                       (outermost; held across HTTP — that is its job)
  └─ TokenProvider gate                 (in-process, ALWAYS before any flock)
       └─ .token.lock flock             (released before the network redeem)
  └─ GRAPH_GATE permit                  (acquired AFTER the token, never before)
       └─ [HTTP]
  └─ cache: RwLock                      (write lock AFTER the HTTP returns; NEVER across it)
```

**Acquire the access token before acquiring a Graph permit.** Otherwise a thread
holding a permit blocks on the token gate while the token holder waits for a permit.
The token endpoint is Entra, not Graph, so it never takes a permit.

`gate` is a **field on `ServerState`**, not a `static` — a `const fn new(4)` static
cannot read `TODO_MCP_GRAPH_CONCURRENCY`. `sem.rs` already exists and is verified.

---

## 6. `tools/list` is frozen — and that creates a first-run trap

The tool set is computed **once at startup** from the granted scope and never
changes; capabilities declare no `listChanged`.

That is simple and cacheable, but it breaks the obvious quickstart: `compose up -d`
(no token → read-only tool set) → `docker run … login` → the write tools never
appear, and `restart: unless-stopped` means nothing ever restarts them into
existence.

**Fix it structurally, not in documentation:**

1. **`serve` refuses to start when there is no usable token**, printing the exact
   `login` command. This is the one refuse-to-start that is correct. README ordering
   becomes `token` → `login` → `up -d`.
2. On a `None|ReadOnly → ReadWrite` transition detected during a later refresh, warn
   and set `restart_required`, and have `todo_account_status`'s **text** say
   `"Restart the server (docker compose restart todo-mcp) to expose the write
   tools."` — the model cannot restart a container, so tell the user the command.

---

## 7. Output contract — global, identical for all tools

1. Every tool declares `outputSchema`; every success carries `structuredContent`.
2. `content[0]` is one text block equal to `serde_json::to_string(&structured)`
   **exactly** — one serialization, no drift. This is [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s `success()`
   helper unchanged.
3. Errors are `{"content":[{"type":"text","text":"<msg>"}],"isError":true}` with
   **no** `structuredContent`.
4. `isError: true` for anything the model can fix by changing arguments (bad list
   name, ambiguous name, expired cursor, throttled, `auth_required`, guard refusal).
   JSON-RPC errors are reserved for unroutable requests — clients render protocol
   errors opaquely and the caller never sees the message.

> **Do not prepend a partial-result banner to `content[0].text`.** An earlier draft
> did. The spec says a tool returning structured content SHOULD also return the
> serialized JSON in a text block for backwards compatibility; prefixing prose makes
> that block unparseable on exactly the responses that most need explaining. The
> banner belongs in `structuredContent.coverage`, which is always present anyway.

**Body limits:** preview 280 B, `get_task` body 8192 B, whole result 262144 B.
HTML→text is dependency-free (drop `<script>`/`<style>` spans; `<br>`, `</p>`,
`</div>`, `</li>`, `<tr` → newline; strip tags; decode the six named entities plus
numeric; collapse whitespace). Truncation is byte-bounded but **char-boundary-safe**
and always ships `*_truncated: true` + `*_bytes_total` — never a bare `"…"`.

---

## 8. `todo_lists` — the first tool

Returns the list catalogue. Annotations: `readOnlyHint: true`,
`destructiveHint: false`, `idempotentHint: true`, **`openWorldHint: true`** (every
call reaches Graph over the internet; the spec default is true and the superseded
plan had this backwards).

> **Counts must be `["integer","null"]` from day one.** Per-list open/overdue counts
> require every list's tasks, and the task machinery does not exist until M4. If M2
> ships a countless schema and M4 adds the field, `outputSchema` changes shape
> between milestones — and `additionalProperties: false` plus a full `required` list
> is exactly what we promised not to churn. Ship the nullable field now, populate it
> when a list is already warm, and add `counts_from_cache: bool`.
>
> **`todo_lists` must never trigger a sync.** It is the cheapest tool and the one a
> model calls first.

List-name resolution (`domain/resolve.rs`, needed here for name → id): exact match
wins, then unique prefix, then unique substring; case- and accent-insensitive.
Ambiguity is an `isError` naming the candidates.

---

## 9. Tests

| File | Pins |
|---|---|
| `mcp_contract.rs` | `initialize` version echo + fallback; unknown method `-32601`; unknown tool `-32602`; malformed JSON `-32700`; notification → `None`; batch handling |
| `http_auth.rs` | wrong bearer 401 + `WWW-Authenticate`; **absent Origin 200**; foreign Origin 403; foreign Host 403; `GET /mcp` 405; oversized body 413; `/healthz` 200 unauthenticated |
| `panic_isolation.rs` | handler panic → 500, worker survives, next request succeeds |
| `http_smoke.rs` | real subprocess, `env_clear()`, port parsed back out of the stderr log line |
| `tools_contract.rs` | `todo_lists` schema is `additionalProperties:false` with full `required`; annotations exact; `content[0].text` parses to exactly `structuredContent` |

**The one test that proves token separation:** a wrong-bearer POST to `/mcp`
produces **zero** requests against the fixture Graph server. Assert the counter.

Fixtures reach the Graph client through
`#[cfg(any(test, feature = "test-fixtures"))] pub fn with_base(...)`, with
`test-fixtures` **off by default** and gate 9 asserting it. `EntraClient` needs the
symmetric hatch. `https_only(true)` stays on in release and is cleared only on that
gated path — never via a runtime env var.

---

## 10. The live Graph probe suite

Run once against the real account; the output goes into `docs/graph-probe.md` with
**all user content redacted** (titles, bodies, list names, checklist items, categories,
account names and emails), ids replaced by consistent pseudonyms, and no raw transcript
ever committed ([`m8-docs-release.md`](m8-docs-release.md) §6). Hidden `probe`
subcommand behind `test-fixtures`.

Precondition: pick the list with the most tasks having both ≥1 completed and ≥1
non-completed task, else refuse.

| Probe | Settles |
|---|---|
| A | ground truth: full `$top=100` walk → `ids_all` / `ids_open` / `ids_done` |
| **B** | `$filter=status ne 'completed'` — honoured, ignored, or hard error? |
| **C** | `$filter=zzNotAProperty eq 'x'` — **a 200 here overrides B and forces the "silently ignored" verdict** |
| D | `$orderby=dueDateTime/dateTime desc` |
| **H** | no `$top` at all — the real default page size. The superseded plan's "10" has no Microsoft source |
| **I** | `PATCH` with `If-Match: W/"bogus"` — honoured (412) or ignored (200)? |
| **F/G** | `PATCH {"dueDateTime":null}` / `{"recurrence":null}` then re-GET — does null actually clear? |
| **J** | `PATCH` a date with `timeZone:"India Standard Time"` — are fallback Windows names accepted? *(M6 blocker)* |
| **K/L** | `Prefer: outlook.timezone` — 400, ignored, or honoured? Does it survive inside `$batch`? |

Client-side filtering ships **in every case** — that is a locked decision. The probe
only decides whether the `$filter` grep gate stays hard-fail or relaxes to a warn.

---

## 11. Settle during M2

- **Which `protocolVersion` each client negotiates.** Record Claude Code, Claude
  Desktop's Code tab and Hermes into the README. This is the evidence base for
  whether a second era is ever worth carrying.
- **Does Hermes Agent connect at all?** Untested. It is the reason HTTP was in scope
  originally, so a failure here is a real finding, not a footnote.
- **`/data` ownership on Linux** — theory settled, measurement not. The answer is
  [`m1-auth.md`](m1-auth.md) §5 and the `Dockerfile` comment; only the Linux run is
  open. [`m7-container.md`](m7-container.md) §8 carries it.
- **Real musl binary size** — set `SIZE_LIMIT` from the first musl build, then
  ratchet. The current ~336 KB host binary is meaningless (LTO dead-strips
  rustls/ureq/tiny_http because nothing calls them yet).

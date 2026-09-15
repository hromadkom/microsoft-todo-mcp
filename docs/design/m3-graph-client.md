# M3 — The Graph client: paging, retry, `$batch`

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Handoff spec. The ureq signatures below were read out of ureq 3.4.0's own source and
docs.rs and compile-checked on rustc 1.97.1; the throttling rules are verbatim Microsoft.
One thing is **not** distilled from the design research: the `@odata.nextLink` origin
check in §3 is new, and closes a token-exfiltration hole the research left open.

**Exit gate.** A 3-page `nextLink` walk returns all 250 fixture tasks, the link followed
**verbatim and origin-checked** · `$top=100` on every collection call · a 429 carrying
`Retry-After: 2` succeeds after ≥2 s with **exactly one** retry · observed concurrency
never exceeds 4 · `TODO_MCP_GRAPH_CONCURRENCY=8` is rejected at startup.

**Files:** `graph/{mod,client,retry,paging,batch,models}.rs`, the
`TODO_MCP_GRAPH_CONCURRENCY` / `TODO_MCP_MAX_PAGES` / `TODO_MCP_TOOL_DEADLINE_MS` / timeout
/ byte-cap rules in `config.rs`, and `tests/{graph_paging,graph_throttle,graph_batch}.rs`.

---

## 1. One agent, and the default that eats every throttle

`ureq::Agent` is `Send + Sync + Clone` and, per `src/agent.rs:47-51`, *"Agent uses inner
Arc… Cloning an Agent results in an instance that shares the same underlying connection
pool… The connection pool contains an inner Mutex."* Build **one**, put it in
`ServerState`, share it across the four workers.

`ureq::Agent::config_builder()` … `.user_agent(concat!("microsoft-todo-mcp/",
env!("CARGO_PKG_VERSION")))` … `.build().into()` (`Config` → `Agent`), with every override
below. `https_only` is cleared **exactly once**, on the cfg-gated fixture path in §4 —
never from config, never from an env var.

| Setting | ureq default | Ours | Why |
|---|---|---|---|
| `http_status_as_error` | **`true`** | **`false`** | See the callout. |
| `https_only` | `false` | `true` | A bearer must never leave over plaintext (§4). |
| `timeouts.*` | all `None` (except `await_100: 1s`) | connect 5 s, global from config | **No timeouts by default** — an unset timeout parks a worker forever. |
| `redirect_auth_headers` | `Never` | keep | Strips `Authorization` across a cross-host redirect. Graph 302s attachment content to blob storage; follow such a `Location` unauthenticated or not at all. |
| `max_idle_connections_per_host` / `max_idle_age` | `3` / `15s` | `6` / `30s` | Four in-flight calls plus a refresh thrash a 3-slot pool; the cache TTL is 120 s, so 30 s keeps sockets warm between bursts. |
| `input_buffer_size` / `output_buffer_size` | **128 KiB each** | `32 KiB` | Source-verified: `LazyBuffers::new` is constructed per connection in `tcp.rs:45` **and again per TLS layer** in `tls/rustls.rs:82`, and `ensure_allocation()` (`buf.rs:95-102`) resizes to full on the first byte — "lazy" only means "until first use". Defaults give 6 conns × 2 layers × 256 KiB ≈ **3 MiB** in a container whose `compose.yaml` sets `mem_limit: 96m`. 32 KiB makes it ≈ 768 KiB, and this is the single biggest RSS lever in the project. |
| `proxy` | `Proxy::try_from_env()` | keep, **document it** | ureq silently honours `HTTPS_PROXY`/`ALL_PROXY`. For a process holding OAuth bearers that must be a decision, not an accident — `docs/operations.md` (M8). |

> **`http_status_as_error(false)` is not a style preference — it is the 2.x→3.x breaking
> change.** ureq 3.x defaults it to `true` (`src/config.rs:867`) and then reports every
> 4xx/5xx as `Err(Error::StatusCode(u16))`, a variant that carries **only the code, not
> the response**. At the default, the Graph error JSON *and* the `Retry-After` header are
> destroyed before your code sees them — the retry loop in §5 loses every input it has,
> and the failure mode is throttle handling that reviews clean and never once honours a
> `Retry-After`.

**No `json` feature.** The shipped manifest takes `features = ["rustls"]` only, and both
`send_json` and `read_json` are `#[cfg(feature = "json")]`, so they do not exist here:
bodies go out as `.send(serde_json::to_string(&v)?)` and come back as `read_to_vec()` +
`serde_json::from_slice` — the same bounded read [`m1-auth.md`](m1-auth.md) §1d does for
Entra, whose *send* side is `send_form`, not JSON. A manifest fact, not a preference.

```rust
// ureq 3.4.0 — signatures verified against source + docs.rs, not remembered.
pub fn get<T>(&self, uri: T) -> RequestBuilder<WithoutBody>     // post/patch/delete symmetric
pub fn config(self) -> ConfigBuilder<RequestScope<Any>>         // per-request timeout override
pub fn with_config(&mut self) -> BodyWithConfig                 // → .limit(u64).read_to_vec()
// Response<Body> IS http::Response<Body> (`pub use ureq_proto::http;`), so
// res.headers().get("retry-after") / .get("request-id") is the plain `http` crate API.
```

---

## 2. Paths, headers, and the byte cap

`graph/` is the only place a Graph path is named ([`../../AGENTS.md`](../../AGENTS.md)) — the base
and the collection paths as `const`s in `graph/mod.rs`, the per-resource paths beside the calls that
use them (`graph/tasks.rs`, `graph/checklist.rs`, [`m6-write-tools.md`](m6-write-tools.md) §8) — and
every path starts `/me`. `/users/{id}/…` is not a policy we enforce, it is a string that appears
nowhere — gate 1 in [`scripts/gates.sh`](../../scripts/gates.sh) fails the build if it ever does.

`GRAPH_BASE` is `https://graph.microsoft.com/v1.0`, `PAGE_SIZE` is 100, and
`is_idempotent(Get | Delete)` is the predicate §5's retry policy hinges on: re-sending a
`POST /tasks` that actually succeeded creates a duplicate, and this API has no
idempotency key.

| Header | Value | Notes |
|---|---|---|
| `Authorization` | `Bearer <access_token>` | Only ever from `TokenProvider` ([`m1-auth.md`](m1-auth.md) §6). The inbound MCP bearer is structurally unreachable from here. |
| `Accept` / `Content-Type` | `application/json` | `Content-Type` on POST/PATCH only. |
| `client-request-id` | 32 hex from the in-house PRNG (§5) | Support correlation; logged, never a secret. It and `request-id` are captured from **every non-2xx** — the only handle a Microsoft support case can use. `User-Agent` is set once on the agent. |
| `Prefer` | `outlook.timezone="<Windows name>", odata.maxpagesize=100` | Timezone half omitted when no Windows name resolves. `odata.maxpagesize` is documented on `todoTask: delta`, and RFC 7240 requires unknown preferences be ignored. Whether `outlook.timezone` is honoured — or 400s — on `/todo` is **UNVERIFIED**: the `List todoTasks` and `Get todoTask` request-header tables list only `Authorization`. Probes K/L in [`m2-mcp-http.md`](m2-mcp-http.md) §10 settle it; M5 owns the consequences. |

**Every collection read carries `$top=100` on the initial URL.** The superseded plan's
claim that the `/tasks` default page size is 10 has **no Microsoft source**; probe H
measures it. `$top=100` is correct either way, which is why the exit gate asserts it. The
**one** exception, and it is deliberate: `todo_account_status`'s optional connectivity check
sends `GET /me/todo/lists?$top=1` ([`m4-cache-read-tools.md`](m4-cache-read-tools.md) §4) — one
request, never paged, nothing cached. Write the assertion against the paging path, not against
every outbound URL, or that probe fails the gate.

**No `$filter`, `$orderby`, `$search`, `$skip`.** Gate 2 as shipped greps `src/graph/` for
`$filter`/`$orderby` only — `$search`/`$skip` are the same policy but are **not** in its
pattern, so a green build is not evidence they are absent. The only verbatim documented
statement about any of them here is on `todoTask: delta`: *"There is limited support for
`$filter` and `$orderby`… The only supported `$filter` expressions are
`$filter=receivedDateTime+ge+{value}` or `$filter=receivedDateTime+gt+{value}`. The only
supported `$orderby` expression is `$orderby=receivedDateTime+desc`… There is no support
for `$search`."* — and `receivedDateTime` is not even a `todoTask` property, which tells you
how much of that page was inherited from the Outlook message docs. Behaviour on
`/todo` is **UNVERIFIED** both ways: one Q&A answer says `$filter=status ne 'completed'`
works, another reports a hard `CollectionNavigationNode operation is not supported in
query filters`. `$select` is unused too — omitting a property makes "absent from the
response" indistinguishable from "cleared on the server" in the cache.

**Byte cap on every read:** `res.body_mut().with_config().limit(cfg.max_response_bytes)
.read_to_vec()`. Otherwise ureq applies `MAX_BODY_SIZE = 10 * 1024 * 1024`
(`src/body/mod.rs:30`) — a tenth of the container's memory limit. Ours defaults to 8 MiB;
the read fails rather than allocating past it → `AppError::Transport`, never buffer-first.

---

## 3. Paging — followed verbatim, origin-checked first

Graph's paging guidance is explicit that the link is opaque: *"continue to call Microsoft
Graph with the `@odata.nextLink` property returned in each response until… no longer
returned"*, and *"Don't try to extract the `$skiptoken`… and use it in a different
request."* So the URL is used exactly as returned — never rebuilt, never re-parameterised,
never split; headers are re-sent, since they are not part of the URL. `TODO_MCP_MAX_PAGES`
(default 50: Graph **read** requests per tool call, retries included, shared by the
catalogue, every `$batch` and every page; one walk also stops at that many pages, 5 000
tasks) bounds the walk, `budget` bounds it in wall-clock (§5).

```rust
/// (items, complete). Running out of budget is NOT an error — it is `complete: false`,
/// which every read tool already carries in `structuredContent.coverage` (M4).
pub fn get_collection(&self, first: &str, budget: &mut Budget)
    -> Result<(Vec<Value>, bool), AppError>;
if let Some(next) = v.get("@odata.nextLink").and_then(Value::as_str) {   // per page
    assert_graph_origin(next)?;   // BEFORE the request is built, i.e. before the bearer
    url = next.to_string();
}
```

> **This is the hole, and it is the whole reason §3 exists.** The research says
> `@odata.nextLink` is *"used exactly as returned"* in one place and *"`Authorization:
> Bearer …` on every request"* in another. Together that is a **server-controlled URL
> fetched with the live Graph access token attached**. `TODO_MCP_GRAPH_BASE` was deleted
> because it *"exfiltrates the live access token"* (§4) — and the same primitive was then
> left in the response body. ureq's `redirect_auth_headers: Never` does not help: this is
> not a redirect, it is a URL we *choose* to fetch.
>
> **Rule, in `graph/paging.rs`, before the request is built:** scheme must be `https`, the
> authority exactly `graph.microsoft.com` — no port, no userinfo — and anything else is
> `AppError::Transport` naming `mask_url(next)`. **The identical check runs on every
> `$batch` sub-response's `nextLink`** (§7); that is the path an implementer forgets,
> because sub-responses do not look like HTTP responses.
>
> Reuse `errors::parse_url` — already written, already tested, and `UrlParts` already
> exposes exactly the fields this needs. Two of its details are load-bearing. (1) `host` is
> lowercased **with the port stripped**, and there is no `authority` field, so
> `host == "graph.microsoft.com"` passes for `…:8443` too — reject a port explicitly.
> (2) Userinfo splits on the *last* `@` (RFC 3986), so a nextLink of the form
> `https://evil.example@graph.microsoft.com/…` really does resolve to our host; reject it
> anyway whenever `username` or `password` is non-empty. Do **not** put a second
> `strip_prefix("https://")` parser beside it: two parsers that can disagree about where
> the host ends is the bug this check exists to prevent.
>
> **Gate 3 is what makes this unbypassable.** `"Authorization"` may appear only in
> `graph/client.rs` and `http.rs`, so there is exactly one place a bearer is attached and
> exactly one place to guard. A second call site fails the build before it can skip it.
>
> None of this contradicts "follow verbatim": you validate, you do not rewrite.

---

## 4. There is no `TODO_MCP_GRAPH_BASE`

`GraphClient::new()` hardcodes `GRAPH_BASE`. The env var **does not exist in any build** —
not cfg-gated, not feature-gated, deleted: it redirected authenticated calls to an
arbitrary host, and a cfg someone can un-gate is not a mitigation. Fixtures reach the
client the way they reach `EntraClient` in M1, through
`#[cfg(any(test, feature = "test-fixtures"))] pub fn with_base(base: &str) -> Self`, which
clears `https_only` too because the fixture is plaintext `tiny_http`. `test-fixtures` is
**not** default (gate 9 asserts `[features]` has no `default` line), and gate 4 asserts
`with_base(` appears in `src/` only as a definition, never as a call. The alternative — a
TLS fixture with a generated cert — puts a second `rustls` in `Cargo.lock` and breaks gate 8.

---

## 5. Retry and throttle

Verbatim from Microsoft's throttling guidance: *"Avoid immediate retries, because all
requests accrue against your usage limits."* · *"Wait the number of seconds specified in
the `Retry-After` header."* · *"If no `Retry-After` header is provided by the response,
we recommend implementing an exponential backoff retry policy."*

```rust
const BASE_BACKOFF_MS: u64 = 500;  const MAX_BACKOFF_MS: u64 = 20_000;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// 429/503/504 retry for every method; 500/502 only when idempotent.
fn retryable(status: u16, m: Method) -> bool {
    match status { 429 | 503 | 504 => true, 500 | 502 => is_idempotent(m), _ => false }
}
/// RFC 9110: delay-seconds or HTTP-date. Graph sends seconds; handle both. Parsed by
/// `DateTime::parse_from_rfc2822`, which needs no chrono `clock` feature (gate 6).
fn retry_after(h: &http::HeaderMap) -> Option<Duration>;
```

`Retry-After` present → honour it **exactly**, capped at `MAX_RETRY_AFTER`: no jitter
(jitter could shorten it), no floor, no ceiling below the server's own number. Absent →
`500 ms × 2^(n-1)` capped at 20 s plus up to 25 % jitter from an eight-line xorshift64*,
no `rand` dependency. `TODO_MCP_MAX_ATTEMPTS` defaults to 4 (1 try + 3 retries).
A **transport** error (reset, TLS failure, timeout) retries on the same budget under the
same idempotency rule as 500/502 — a `POST /tasks` that died on the wire may still have
created the task, and there is no idempotency key to make asking safe.

```rust
pub fn send(&self, req: &GraphReq, budget: &Budget) -> Result<GraphRes, AppError> {
    loop {
        let token = self.tokens.access_token()?;      // token BEFORE permit — m2 §5
        let res = {
            let _permit = self.gate.acquire_timeout(budget.remaining())
                .ok_or(AppError::Throttled { retry_after_secs: 1 })?;  // never 0 → "retry now"
            self.send_once(req, &token)               // permit scopes EXACTLY this call
        };                                            // …and is dropped here
        // …classify, compute `wait`…
        if Instant::now() + wait > budget.deadline {
            return Err(AppError::Throttled { retry_after_secs: wait.as_secs() });
        }
        std::thread::sleep(wait);                     // no permit is held across this
    }
}
```

> **The permit must not be held across the sleep, and this is the bug that ships
> otherwise.** The obvious structure acquires one permit and loops inside it. With
> `GRAPH_CONCURRENCY = 4` and `WORKERS = 4`, four requests that each catch one
> `Retry-After: 120` park **every permit in the process for two minutes**: `acquire_timeout`
> returns `None` and every unrelated tool call fails while the server does nothing. The
> permit scopes one `send_once`, the sleep happens outside it, and `src/sem.rs` is written
> for exactly that — `Permit` is RAII and its `Drop` restores the count on an unwind.
>
> The other half of the fix is a deadline for **every** caller.
> `TODO_MCP_TOOL_DEADLINE_MS` (default 25 000) is stamped into `Budget` at the top of
> every tool call, not just the cold sync, so `now + wait > deadline` fails fast rather
> than blocking a worker past the point the MCP client has given up.

`Budget { deadline: Instant, requests_left: u32, max_pages: u32 }`; `requests_left` is
**derived, not configured** — M4 sets the cold-sync budget from `TODO_MCP_MAX_PAGES` rather
than adding a knob that can disagree with the first. `send_once` goes through an injectable
transport seam, so the whole retry table is testable with no network and no real sleeping.

**Do not add `backon`.** The load-bearing rule — honour `Retry-After` exactly, never early
— is not a backoff policy but a response-header read *inside* the loop. It also pulls
`fastrand`, and its blocking half is the lesser-used side of an async-first crate.

---

## 6. The concurrency ceiling

Every outbound Graph call takes a permit from **one** semaphore, so the ceiling is
per-mailbox rather than per call site: two simultaneous cache misses plus a write would
otherwise put nine requests in flight. (Total volume is nowhere near the
10 000-per-10-minutes budget — a cold fill is roughly N+1 requests for N lists, typically
5–15, then one refill per 120 s TTL and a trickle of one-request writes.) `gate` is a
**field on `ServerState`**, never a `static`: a `const fn new(4)` static cannot read
`TODO_MCP_GRAPH_CONCURRENCY`, and the knob is rejected-not-clamped, so the value has to
arrive at construction time ([`m2-mcp-http.md`](m2-mcp-http.md) §5 lists it there). Whether
`GraphClient` borrows that field or shares an `Arc` of it is an implementation detail; what
must not exist is a *second* gate, so §5's `self.gate` has to resolve to this one.
`src/sem.rs` already exists and is verified — its own test asserts peak in-flight reaches
exactly the capacity with 32 contenders, and a permit is returned on unwind. Keep
`const fn new`; stop advertising a static.

**Lock order is fixed and lives in [`m2-mcp-http.md`](m2-mcp-http.md) §5.** The rule M3
must not violate: **acquire the access token before acquiring a Graph permit** — otherwise
a permit holder blocks on the token gate while the token holder waits for a permit. The
token endpoint is Entra, not Graph, so it never takes a permit.

> **Never cite "four concurrent requests" as a documented fact about the To Do API.**
> Microsoft's throttling-limits page does publish it, under *Outlook service limits →
> Limits per mailbox*, for v1.0 and beta: *"10,000 API requests in a 10-minute period"*,
> *"Four concurrent requests"*, *"150 megabytes (MB) upload (PATCH, POST, PUT) in a
> 5-minute period"* — and the batching prose on the same page corroborates it:
> *"Microsoft Graph sends the Outlook service up to four individual requests from the
> batch at a time… so the execution of that batch stays within Outlook's concurrency
> limits for the same mailbox."* But that page's resource table lists the To-Do API under
> the **legacy** `outlookTask`/`outlookTaskFolder`/`outlookTaskGroup` types, not
> `todoTask`. That `/me/todo/*` is governed by the same limit is a strong **inference** from
> the shared Exchange backend and **UNVERIFIED** as an explicit statement. Write "conservative
> inference" in the code comment — the honest word is what stops the next reader raising it.

> **`Cell` and `RefCell` cannot live in this struct.** The research sketches
> `GraphClient { rng: Cell<u64>, stats: RefCell<GraphStats>, tz_mode: Cell<TzMode> }`,
> which was fine while [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp) wrapped everything in one `Arc<Mutex<McpServer>>`. M2
> deletes that mutex, so `ServerState` is shared across four worker threads and must be
> `Sync` — and `Cell`/`RefCell` are not. Use `AtomicU64` for the PRNG and a poison-absorbing
> `Mutex<GraphStats>`: a second-order cost of `&self`, on no hot path, and worth paying.

---

## 7. `$batch` — the cold-start answer, and why it is not delta

`POST /v1.0/$batch`, **at most 20 sub-requests** (documented hard cap; `BATCH_MAX` is a
compile-time constant — no reason to tune a number Graph fixes). Each sub-request carries a
**relative** `url` and its own `headers`, so `Prefer` rides along; each sub-response
carries its own `status` and headers. A typical user's 5–15 lists all yield their first
page in **one HTTP round trip**, which is what makes a bounded cold sync possible at all.
Delta is cut: `deltaLink`, `@removed`, 410-Gone and `syncStateNotFound` are four failure
modes to earn a saving the TTL cache already gets.

- **A batch is one HTTP call**: one permit, one `attempt` for §5's purposes.
- **Pair sub-responses to sub-requests by `id`, never by array index.** Nothing in the
  research says Graph preserves order, and matching by `id` makes the question moot —
  index-matching would silently file one list's tasks under another.
- **A sub-response 429 is never retried automatically.** Collect the failures, wait the
  **largest** `retry-after` among them, and re-issue *only those* sub-requests in a
  follow-up batch — per Microsoft's batching guidance; retrying the whole batch re-runs
  the sub-requests that succeeded.
- **A per-sub-request failure is data, not an error**: that list is absent with
  `complete: false`, and M4 renders it into `coverage`.
- **`nextLink` continuations are never batched.** They are absolute, `$batch` needs
  relative, and rewriting a URL we promised to follow verbatim is exactly what §3 refuses
  to do. They are plain sequential `GET`s — and every sub-response's `nextLink` goes
  through `assert_graph_origin` before it is used.

> **UNVERIFIED: that `$batch` works against `/me/todo/*` at all.** Batching is documented
> generically — 20 sub-requests, relative url, per-sub headers, per-sub status — but was
> never exercised against To Do endpoints, and the cold-start latency story rests on it.
> Probe L ([`m2-mcp-http.md`](m2-mcp-http.md) §10) sends a 3-list batch with `Prefer` in
> each sub-request; check the sub-statuses and `Preference-Applied`. **Fallback:**
> sequential first pages under the same budget, raising cold start to N+1 round trips and
> making `complete: false` the common first answer — a latency regression, not a redesign.

---

## 8. ETag: capture it, never send `If-Match`

`todoTask` and `todoTaskList` have **no documented `etag`/`changeKey` property**, yet real
responses carry `"@odata.etag":"W/\"s8/ERWT3WEeFpBGD0bDgAA+TWq9g==\""`. The `Update
todoTask` and `Delete todoTaskList` request-header tables list **only** `Authorization`
plus `Content-Type` — `If-Match` is documented nowhere for To Do.

**Do** store `@odata.etag` on the cached task/list and surface it as `etag` in
`todo_get_task`, so a caller can detect drift. **Do not** send `If-Match` in v1: an
undocumented conditional header is either ignored — false safety, worse than none — or
honoured, in which case every write against a ≤120 s-stale cache becomes a `412` the model
cannot recover from without a refetch it never asked for. Last-writer-wins is what the To
Do apps do. Probe I ([`m2-mcp-http.md`](m2-mcp-http.md) §10) PATCHes with `If-Match:
W/"bogus"`: 412 means honoured and a v2 opt-in becomes defensible, 200 means ignored and
the door stays shut. Until then this is **UNVERIFIED** — say so in the tool description
rather than implying optimistic concurrency exists.

---

## 9. Config surface M3 owns

Every row is a `tests/config_refusals.rs` case, and no refusal message echoes a value.

| Variable | Default | Validation |
|---|---|---|
| `TODO_MCP_GRAPH_CONCURRENCY` | `4` | `1..=4`; **>4 rejected, not clamped** — a silent clamp hides an operator's wrong mental model, and this one is the exit gate. |
| `TODO_MCP_MAX_PAGES` | `50` | Graph **read** requests per tool call, retries included; `1..=10000`, `0` refused; a list above 5 000 tasks never completes at the default; mutations are not counted (m6). |
| `TODO_MCP_MAX_ATTEMPTS` | `4` | `1..=8` (1 try + 3 retries). |
| `TODO_MCP_HTTP_TIMEOUT_MS` | `20000` | `1000..=120_000`; ureq `timeout_global`, shared with the Entra client. |
| `TODO_MCP_TOOL_DEADLINE_MS` | `25000` | `1000..=120_000`; wall-clock ceiling on one tool call — `Budget.deadline`. |
| `TODO_MCP_MAX_RESPONSE_BYTES` | `8388608` | single Graph body read; exceeded → `Transport`, never buffered. |

Deliberately compile-time constants, **not** env vars: `CONNECT_TIMEOUT_MS` (5 000),
`PAGE_SIZE` (100 — the exit gate asserts it), `BATCH_MAX` (20 — Graph's cap), the backoff
bounds, the idle-pool numbers, the 32 KiB buffers. Each has exactly one defensible value.

---

## 10. Tests

Fixture-driven over the hand-rolled `tiny_http` Graph server in `tests/common/mod.rs`; no
`wiremock`, no dev-dependencies. The concurrency assertion belongs on the **fixture** side
— have it track peak in-flight requests; asserting it client-side only re-tests `sem.rs`.

| File | Pins |
|---|---|
| `graph_paging.rs` | 3-page walk returns all **250** fixture tasks in order; every request carries `$top=100`; the fixture's `@odata.nextLink` is echoed back **byte-identically** in the follow-up request line. `MAX_PAGES = 2` against the same fixture → `complete == false`, no error, first two pages present; a 9 MiB body is refused, not buffered |
| `graph_paging.rs` | `nextLink` pointing at `http://…`, `https://evil.example/…`, `https://graph.microsoft.com.evil.example/…`, `https://graph.microsoft.com:8443/…` and `https://x@graph.microsoft.com/…` → **all five refused**, and the fixture's foreign-host counter stays at **zero** |
| `graph_throttle.rs` | 429 + `Retry-After: 2` → success after **≥2 s** with **exactly one** retry (assert the fixture's request count, not just the outcome); `Retry-After: 300` under a 25 s deadline → immediate `Throttled`, zero further requests, elapsed < 1 s |
| `graph_throttle.rs` | 503 with no header → exponential backoff, attempts == `MAX_ATTEMPTS`; 500 on `POST` **not** retried; 500 on `GET` retried |
| `graph_throttle.rs` | **while one caller sleeps on a `Retry-After`, an unrelated caller still acquires a permit** — the permit-drop invariant, and the test that fails if §5 is written the obvious way |
| `graph_batch.rs` | 20 sub-requests parse per-sub `status` and headers; a sub-429 is retried in a follow-up batch containing **only** the failed ids, after the largest `retry-after`; an off-origin sub-response `nextLink` is refused without a request being made |
| `config_refusals.rs` | `TODO_MCP_GRAPH_CONCURRENCY=8` refused at startup naming the range; `=0` refused; `=4` accepted |

---

## 11. Settle during M3

- **`$filter`/`$orderby` on `todoTask`** — run probes B/C/D and write the verdict into
  `docs/graph-probe.md`, redacted per [`m8-docs-release.md`](m8-docs-release.md) §6 (the M2 probe
  suite creates that file). Client-side filtering
  ships either way; the probe only decides whether gate 2 stays hard-fail. **Probe H** in the
  same run settles the real default page size: the "10" in the plan has no source, so record
  the measured number.
- **Probe I — is `If-Match` honoured?** Until it runs, "we do not send it" is a decision,
  not a limitation. Do not describe `etag` as optimistic concurrency.
- **Probes K/L — `Prefer: outlook.timezone` on `/todo`, and inside a `$batch`.** If it is
  rejected as malformed rather than ignored, every read 400s. Send it on one GET and check
  for a 4xx **before** wiring it into every request; §7 covers the batch fallback.
- **A transient 500 on `POST /tasks` is reported as failure even though the task may
  exist.** No idempotency key exists here and v1 does not reconcile, so the model will
  likely retry and duplicate. A post-failure reconciliation read (GET the list, match on
  title + `createdDateTime` within 30 s) is the obvious fix — M6 owns the decision.
- **Cold-sync convergence has never been measured against a large mailbox.** M4 sets the
  budget from `TODO_MCP_MAX_PAGES`; if a ~40-list account needs more than two or three
  calls to converge, the defaults are wrong, not the design —
  [`m4-cache-read-tools.md`](m4-cache-read-tools.md).
- **gzip** is off in the shipped manifest and `todoTask` collections are verbose JSON: a
  size-versus-bandwidth experiment for after the musl size gate has a real number.

# M4 — TTL cache and the read tools

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Handoff spec. The cache is the only place this server keeps task content, and it keeps it
**in memory only** — nothing here touches disk. Four things look wrong until you know why:
the refill mutex is held across a 20 s HTTP sync, the cache lock alone does *not* absorb
poison, `todo_lists` may never fetch a task, and a cursor is refused after an unrelated create.

**Exit gate.** A second identical `todo_search_tasks` inside the TTL issues **zero**
Graph requests · a cursor is rejected on each of the seven conditions with its
verbatim text · a budget-exhausted sync returns `complete: false` with a full
`coverage` block and the next identical call converges · `todo_account_status`
makes **no `/me` call** · whole-list LRU eviction never underflows `total_tasks` ·
`content[0].text` still parses to exactly `structuredContent` on a partial result.

**Files:** `cache.rs`, `tools/{mod,schema,render,read,cursor}.rs`, `domain/filter.rs`, and
`domain/resolve.rs` — M2's name→id resolver, extended here with the id→list lookup
`todo_get_task` needs. Read [`m3-graph-client.md`](m3-graph-client.md) (paging, `$batch`,
retry, the nextLink origin check) and [`m2-mcp-http.md`](m2-mcp-http.md) §5, §7, §8 first;
[`m5-agenda-timezone.md`](m5-agenda-timezone.md) builds `todo_agenda` and
`domain/datetime.rs::resolve()` — a **different** `resolve` — on top of this.

---

## 1. The cache

```rust
pub struct Cache {                     // in memory only — nothing here touches disk
    pub mailbox_id: String,            // "mbx_<16 hex>" | "mbx_pending" | "mbx_unknown"
    pub generation: u64,               // bumped by EVERY mutation; binds cursors (§6)
    catalogue: Option<Catalogue>,      // { fetched_at: Instant, lists: Vec<TaskList> }
    tasks: HashMap<ListId, ListTasks>, // { fetched_at, complete, tasks: Vec<Task>, index }
    total_tasks: usize,                // MUST equal the sum of tasks[*].tasks.len()
    lru: VecDeque<ListId>,             // MRU at the back
}   // ListTasks.complete == false: the page walk was cut short by the budget, not bad data
```

| Knob | Source | Default | Rule |
|---|---|---|---|
| Entry TTL | `TODO_MCP_CACHE_TTL_SECONDS` | `120` | catalogue and each list expire **independently**, each from its own `fetched_at`; `0` disables caching |
| Eviction | `TODO_MCP_CACHE_MAX_TASKS` | `5000` | evict **whole lists** from the LRU front until `total_tasks <= max`; never the list being served, never the catalogue |
| Stored body | `BODY_STORE_BYTES` (constant) | `4096` | `body.content` truncated **at ingest** — the biggest cache-side RSS lever, second only to the 32 KiB ureq buffers ([`m3-graph-client.md`](m3-graph-client.md) §1) — `body_bytes_total` retained, and `todo_get_task` re-fetches for a full body, so nothing is lost |

### 1.1 `mailbox_id`, and why `account.json` does not exist

`openid`/`profile` are not requested and `/me` needs `User.Read`, so **there is no identity
source from Graph**. `derive_mailbox_id(lists) -> "mbx_<16 hex>"` is FNV-1a (six lines, no
dependency) of the `defaultList` id — mailbox-scoped, stable, no extra scope, changing
exactly when the user logs into a different mailbox, which is when every cache entry and
cursor *should* invalidate. `"mbx_pending"` until the first catalogue fetch, `"mbx_unknown"`
when the mailbox has no `defaultList` (§10); a **cache label, never a security boundary**
(say so at M8).

> **`account.json` is deleted from the design — do not create it, and no document
> may list it under `/data`.** Persisting `mailbox_id` helps neither consumer: it labels
> a **memory-only** cache that is empty at boot, and binds cursors with a 900 s TTL that
> cannot survive a restart either. It is also not swept by `logout`, so after logout →
> login-as-a-different-account it is *stale* ([`m1-auth.md`](m1-auth.md) §5).

### 1.2 The one lock that resets

> **`cache: RwLock<Cache>` treats poison as RESET, not absorb.** Every other lock here
> absorbs (`.lock().unwrap_or_else(|e| e.into_inner())`) because its state has no
> cross-field invariant. `Cache` has three: `total_tasks` equals the sum of the per-list
> vectors, `lru` holds exactly the keys of `tasks`, each `ListTasks.index` maps into its own
> vector. A panic under the write guard can leave `total_tasks` incremented but `tasks`
> unmodified; absorbing that poison serves a broken cache, and the next eviction does
> `total_tasks -= 1` on a count that was never real.
>
> ```rust
> fn cache_write(&self) -> RwLockWriteGuard<'_, Cache> {          // and cache_read()
>     self.cache.write().unwrap_or_else(|p| { let mut g = p.into_inner();
>         logger::warn("cache poisoned by a panic; clearing");
>         g.reset_preserving_mailbox_id();                        // bumps generation
>         self.cache.clear_poison(); g })  }  // clear_poison: stable since 1.77
> ```
>
> Resetting bumps `generation`, so outstanding cursors are refused loudly instead of
> paginating over a cache that just emptied — the exception flagged in
> [`m2-mcp-http.md`](m2-mcp-http.md) §4.

---

## 2. No write-through — one invalidation rule instead

Write-through was cut together with the delta cache by an explicit locked decision; the
reasoning is the value. **The rule, entire:** any mutation that returns success invalidates
that list's entry (`tasks.remove(&list_id)`, decrement `total_tasks`, drop it from `lru`)
and bumps `generation`. **Any mutation that returns 5xx or times out invalidates it too.**
M6 owns calling `invalidate_list()` on **both** outcomes; M4 owns the function and the bump.

Four lines instead of a five-row mutation table; create-then-search stays consistent because
the next read refetches, and `generation` still bumps, so cursor binding is unaffected. What
it removes is the class of *confidently wrong cache after an unknown write outcome*: with
write-through, a create whose response never arrived leaves the cache asserting a state
nobody verified, and the model reports it as fact. Cost: one list fetch per write. Saving:
~150 LOC and a test file.

---

## 3. Bounded cold sync, and the `coverage` block

An all-lists query needs every task in every list. Sequentially at `$top=100` that is *N × P*
round trips at 150–400 ms each, and the client's UX dies long before its timeout does. So the
first call gets a **budget**, not a promise: `sync_all(&self, g: &GraphClient, budget: &mut
Budget) -> SyncOutcome`, over M3 §5's `Budget { deadline, requests_left, max_pages }`.

| Bound | Source | Default |
|---|---|---|
| wall clock | `TODO_MCP_SYNC_TIMEOUT_MS` | `20000` (20 s) |
| total Graph **read** round trips per tool call, retries included | **derived**, `= TODO_MCP_MAX_PAGES` (M3) | `50` |
| pages in any one collection walk | ≤ `TODO_MCP_MAX_PAGES` (M3), and the walk also stops when the shared read budget is spent | `50` |
| `$batch` sub-requests | `BATCH_MAX` (M3, a **constant**) | `20` (Graph's documented hard cap) |

The request budget is **derived, not configured**: the catalogue, every `$batch` request
and every list's pages share one allowance of `TODO_MCP_MAX_PAGES` read requests per tool
call — one knob instead of two whose interaction nobody can predict. `deadline` is `min(now + sync_timeout, tool_deadline)`:
[`m3-graph-client.md`](m3-graph-client.md) §5 stamps `TODO_MCP_TOOL_DEADLINE_MS` (25 s)
into `Budget` for **every** tool call, and the sync must finish inside it with room left
to render — never extend it.

Write tools' mutations are not counted against this allowance. The reads a write tool
makes (the list catalogue, the one-time id lookup, `todo_manage_checklist`'s checklist
read) are charged here; the mutations themselves run on a separate budget of
mutations × (`TODO_MCP_MAX_ATTEMPTS` + 1) ([`m6-write-tools.md`](m6-write-tools.md)).

1. **Catalogue** — `GET /me/todo/lists?$top=100` + walk; derive `mailbox_id`.
2. **Priority** — `defaultList`; then warm-but-expired lists (the ones the user actually
   uses); then the rest by `displayName`; **`flaggedEmails` last**, typically the largest
   and the least likely answer.
3. **First pages via `$batch`**, chunks of 20, one per stale list — that `$batch` works
   against `/me/todo/*` at all is **UNVERIFIED** (probe L; the fallback is sequential first
   pages under the same budget, [`m3-graph-client.md`](m3-graph-client.md) §7).
   Continuations are sequential plain `GET`s, never batched (below).
4. Budget check at every loop head; an unfinished list keeps `complete: false`.

> **`@odata.nextLink` continuations are deliberately not batched.** `$batch` needs a
> **relative** url; `nextLink` is absolute and server-controlled, followed verbatim and
> origin-checked before a bearer is attached ([`m3-graph-client.md`](m3-graph-client.md)
> §3, §7). Rewriting it relative discards exactly the property that makes attaching a
> token to it safe — to save a round trip on the tail of a tail.

> **`refill: Mutex<()>` is held across the whole HTTP sync. That is its job.** It is the
> outermost lock in the order fixed by [`m2-mcp-http.md`](m2-mcp-http.md) §5: a second
> thread that misses blocks on it, then **re-reads the cache on acquiring it** and usually
> finds it warm — one sync for N concurrent cold callers. The rule it must not break is the
> other one: the `cache` write guard is taken **after** the HTTP returns and **never** held
> across it. Holding `refill` across the network is single-flight; holding `cache` there
> serialises every reader behind Graph.

**Nothing here is an error.** `complete: false` never sets `isError`. A partial sync leaves
what it fetched warm, so the next identical call skips those lists and spends its whole
budget on what is missing — converging in two or three calls.

```json
{"complete": false,
 "coverage": {"lists_total":14,"lists_complete":9,"lists_partial":["Flagged emails"],
   "lists_missing":["Work","Reading","Someday","Archive"],"tasks_seen":812,
   "stopped_because":"sync_timeout","elapsed_ms":20014,"graph_requests":27,
   "banner":"PARTIAL RESULT — 9 of 14 lists synced within 20s (stopped: sync_timeout). Missing: Work, Reading, Someday, Archive. Call again to continue.",
   "retry_hint":"Call the same tool again with the same arguments; the cache kept everything already fetched and the next call resumes with the lists still missing."}}
```

`stopped_because` ∈ `complete | sync_timeout | request_budget | page_cap | cache_cap |
throttled`. When `complete == true` both list arrays are `[]`, `stopped_because` is
`"complete"` and `banner` is `null`. **Every key is always present**, so `outputSchema`
keeps `additionalProperties:false` + a full `required`.

> **The banner lives in `coverage`, never in `content[0].text`.** An earlier draft
> prepended `"<banner>\n<json>"` to the text block. The spec's compatibility clause is
> that *"a tool that returns structured content SHOULD also return the serialized JSON
> in a TextContent block"* — prefixing prose makes that block unparseable for a client
> on the compatibility path, on **exactly** the responses that most need explaining.
> `content[0].text` stays byte-identical ([`m2-mcp-http.md`](m2-mcp-http.md) §7).

---

## 4. The read tools

All four annotation hints are written explicitly on each: `readOnlyHint: true`,
`destructiveHint: false`, `idempotentHint: true`, **`openWorldHint: true`** — every
call reaches `graph.microsoft.com`, and the spec default is true. Annotations are
untrusted; read-only is enforced by there being no write path in `tools/read.rs`.

**`todo_lists`** shipped in M2 with its per-list counts typed `["integer","null"]` plus
`counts_from_cache: bool`, precisely so M4 adds no field — it fills them in **only for lists
already warm** ([`m2-mcp-http.md`](m2-mcp-http.md) §8).

> **`todo_lists` must never trigger a sync — `include_counts: true` included.** It is
> the cheapest tool and the one a model calls first, usually only to resolve a name. If
> counts could pull a cold sync, the first call of every session would spend a 20 s
> budget answering *"what are my lists"*. Counts are a by-product of a warm cache, never
> a reason to warm it; the fixture test asserts **zero** `/tasks` requests here.

**`TaskSummary`** is the item type of every multi-task result — here and in `todo_agenda`'s
sections — and no other spec defines it: `task_id, list_id, list_name, title, status,
importance, due, due_unresolved, start, completed_at, created_at, modified_at, categories,
has_checklist, checklist_open, checklist_total, is_recurring, reminder_at, body_preview,
body_truncated, body_bytes_total, etag`. `additionalProperties:false`, all of it in
`required`, nullables typed `["string","null"]` rather than omitted — omission and null must
never be confusable (§5). `checklist_open`/`checklist_total` are **always `null` here**: the
collection is not expanded on `/tasks`; only `todo_get_task` fills them.

**`todo_search_tasks`** — the universal read. Arguments: `list`; `query` (≤200 chars,
case-insensitive substring over title, body text and category names, whitespace terms
ANDed); `status` (default `open` = "not `completed`"); `due`
(`any|overdue|today|tomorrow|this_week|next_7_days|no_due_date|has_due_date`);
`due_from`/`due_to` (`^\d{4}-\d{2}-\d{2}$`); `importance`; `categories` (≤20); `sort`
(default `due_asc`, **nulls last**); `limit` (1–200, default 50); `cursor`;
`timezone`. Mutually exclusive and refused pre-flight with **zero** HTTP: `cursor` ×
any filter key, `due` × (`due_from`|`due_to`). Output: `{account_id, timezone,
complete, coverage, truncation, total_matched, returned, next_cursor|null,
tasks:[TaskSummary]}`.

The **fast path is the exit gate**: with `list` given and warm — or no `list` and the
catalogue plus every list warm — the call is **zero Graph requests**, a cache read plus a
predicate pass plus a sort. `domain/filter.rs` holds the predicates; its date arms call
`domain/datetime.rs::resolve()` and gain correctness at M5, not shape.

**`todo_get_task`** — `task_id` (required), `list` (skips a lookup; if omitted the id is
resolved from cache, and only then does an all-lists sync run), `body_format`
(`text|html|none`, default `text`), `timezone`. Returns every `TaskSummary` field plus
`body`, `body_content_type`, `recurrence` (raw `patternedRecurrence` or `null`),
`checklist_items[]`, `linked_resources[]`, `attachments[]` (**metadata only, never
bytes**), `has_attachments`, `etag`, and `web_link: null` described as *"Microsoft To Do
exposes no per-task deep link through Graph."* — stated rather than invented. Cost:
**2 requests** warm (`GET task`, `GET checklistItems`), 3 when `has_attachments`.

**`todo_account_status`** takes one optional `check_connectivity: false`; true issues
exactly one `GET /me/todo/lists?$top=1`, false touches no network — which is why it is the
one tool that still works under `Grant::None`. It reports the grant, `scopes_granted[]`,
`write_tools_enabled`, `restart_required` + `restart_reason|null`, the token horizon,
`login_command`, `timezone` + `timezone_mode`, and three blocks: `cache` (`mailbox_id`,
lists/tasks cached, `max_tasks`, `ttl_seconds`, `generation`, oldest entry age, hits,
misses), `graph` (requests, `throttled_429`, retries, last status and `Retry-After`) and
`server` (version, protocol version, uptime) — never the access or refresh token, the MCP
bearer, or more than the last 4 characters of the client id. On a `None|ReadOnly → ReadWrite`
transition its **text** says `"Restart the server (docker compose restart todo-mcp) to expose
the write tools."` — the model cannot restart a container
([`m2-mcp-http.md`](m2-mcp-http.md) §6).

> **It never calls `/me`, and the exit gate asserts it.** `/me` needs `User.Read`; this
> server requests one scope, and `/users/{id}/…` exists nowhere
> ([`../../AGENTS.md`](../../AGENTS.md), gate 1). So `account_id` is the literal string
> **`"default"`** — there is no derivation path and none is invented — and the tool reports
> `identity: "not requested (no openid scope)"`. The FNV label from §1.1 is surfaced
> separately as `cache.mailbox_id`, where nobody mistakes it for an identity.

---

## 5. Why every filter is client-side

Locked decision: all filtering, sorting and searching happens in `domain/filter.rs`, over
data already in the cache; `$filter`, `$orderby`, `$search` and `$skip` never reach a URL
(gate 2). Behaviour on `/todo` is **UNVERIFIED** in both directions and the evidence —
Microsoft's verbatim `todoTask: delta` statement, and the two Q&A answers that contradict
each other — is preserved in [`m3-graph-client.md`](m3-graph-client.md) §2 rather than
re-derived here. Probes B/C/D ([`m2-mcp-http.md`](m2-mcp-http.md) §10) decide only whether
gate 2 stays hard-fail; **client-side ships either way.**

> **`$select` is never used either, and the cache is the reason.** It is documented as
> supported and would shrink every response. But omitting a property makes *"absent
> from the response"* indistinguishable from *"cleared on the server"* the moment that
> response is merged into a cache entry — and this design has an explicit `clear_*`
> vocabulary (M6) whose entire point is that clearing is real. Full projections only.

---

## 6. Cursor pagination — `todo_search_tasks` only

Opaque to the model, self-describing to the server, **loud** when the world moved.

```rust
#[derive(Serialize, Deserialize)]
pub struct Cursor {                 // encoded: JSON -> base64url (unpadded) -> "c1_" prefix
    pub v: u8,     // format version — 1        pub o: usize,  // offset into the matched, sorted set
    pub a: String, // mailbox_id (§1.1)         pub n: usize,  // page size that produced it
    pub q: u64,    // FNV-1a of canonical args  pub t: i64,    // issued_at, unix seconds
    pub g: u64,    // cache.generation at issue
}
```

~90 bytes of JSON ⇒ ~120 characters. The codec is hand-rolled over `A–Za–z0–9-_`, ~40 lines —
hex is dependency-free too but doubles the length, and models copy these strings back verbatim
(§10). **`q`** canonicalises the filter arguments in a fixed key order (`list, query, status,
due, due_from, due_to, importance, categories(sorted), sort, timezone`) with defaults
materialised, then FNV-1a's the bytes; `limit` is **excluded**, because changing page size
mid-run is legal and `n` records what happened. **TTL is a constant, not an env var:**
`CURSOR_TTL_SECONDS = 900` is quoted verbatim in rejection 5 below, and configurability would
make that text a lie. `t` and the `now` it is compared against are this milestone's only
wall-clock reads — both from the injected `Clock`, never `chrono::Utc::now` (`clippy.toml`
disallows it outside `logger.rs`); everything else is a monotonic `Instant`.

Checks run **in this order**, each an `isError` carrying exactly this text:

| # | Condition | Verbatim refusal |
|---|---|---|
| 1 | any filter key present alongside `cursor` | `Pass "cursor" alone (optionally with "limit") to continue a search, or pass filters alone to start a new one. Do not pass both.` |
| 2 | bad prefix / base64 / JSON | `Invalid cursor. Cursors come from the "next_cursor" field of a previous todo_search_tasks result and cannot be constructed by hand. Re-run the search without a cursor.` |
| 3 | `v != 1` | `Cursor was issued by a different server version. Re-run the search without a cursor.` |
| 4 | `a != mailbox_id` | `Cursor belongs to a different account. Re-run the search without a cursor.` |
| 5 | `now - t > 900` | `Cursor expired after 15 minutes. Re-run todo_search_tasks with the same filters to start a fresh page.` |
| 6 | `g != cache.generation` | `Cursor is stale: tasks were created, updated or deleted since this page was produced. Re-run todo_search_tasks with the same filters — results have moved.` |
| 7 | `q != hash(current args)` | `Cursor does not match these search arguments. Pass "cursor" alone to continue a search, or pass filters alone to start a new one.` |

`next_cursor` is `null` when `o + returned >= total_matched`.

> **Generation-strict looks over-strict. It is the whole point.** `generation` is
> bumped by every mutation *and* by a cache reset (§1.2), so creating one task
> invalidates every outstanding page. The alternative — best-effort pagination over a
> shifting offset — silently **skips or duplicates** tasks and the model presents the
> result as complete. A loud "re-run" costs ~1 ms against a warm cache.

---

## 7. The response cap — an ordered, per-tool ladder

Serialize `structuredContent` once and measure. Over `TODO_MCP_TOOL_RESULT_MAX_BYTES`
(262144), apply the steps below **in order**, re-measuring after each, bounded to 8 iterations.
`todo_get_task` also carries unconditional render-time caps: `checklist_items` ≤ **200**,
`linked_resources` ≤ **50**, `attachments` ≤ **50** (each with `*_truncated` + `*_total`), and
`body` ≤ 8192 B.

| Step | `todo_search_tasks` | `todo_lists` | `todo_get_task` |
|---|---|---|---|
| 1 | drop every `body_preview` → `null`, set `body_omitted_for_size: true` | — (no bodies) | halve the three sub-collections (floor 10 / 5 / 5); flags already set |
| 2 | halve `returned`, re-issuing `next_cursor` at the **new** boundary so nothing is lost | drop all per-list counts → `null`, `counts_from_cache: false` | **terminal:** drop `body` → `null`, `body_truncated: true`, `body_bytes_total: <n>` |
| 3 | repeat step 2 down to a floor of 1 item | — | — |
| 4 | still over ⇒ `isError` | still over ⇒ `isError` | still over ⇒ `isError` |

> **The raw design had one ladder for all tools, and it only works for one of them.**
> "Halve the item count and re-issue `next_cursor`" is meaningless for `todo_lists`, which
> has no cursor, and impossible for `todo_get_task`, which has exactly one item — and a
> 260 KiB single task was legal (`body` is capped at 8 KiB; the three sub-collections were
> not) with no defined remedy at all. Hence one column per tool, plus the hard caps above.

`todo_agenda`'s column is the same shape with `max_per_section` in place of the cursor
(previews → lower `max_per_section` + `sections.*.truncated` → floor 1 → `isError`) and is
written out in [`m5-agenda-timezone.md`](m5-agenda-timezone.md) §7. No tool falls through.

The terminal `isError` names the task and the escape hatch: *"Result exceeds the 256 KiB
response cap even for a single task (task `<id>`, ~`<n>` KiB). Call todo_get_task with
body_format:"none" to inspect it."* `structuredContent.truncation` is **always present**
so the schema stays stable: `{"applied":true,"steps":["bodies_dropped","page_halved"],
"original_items":50,"returned_items":25,"bytes":251003}`, or `{"applied":false,…}`. Truncated
values ship `*_truncated` + `*_bytes_total`, never a bare `"…"` (m2 §7 owns both helpers).

---

## 8. Environment surface M4 owns

| Var | Default | Validation |
|---|---|---|
| `TODO_MCP_CACHE_TTL_SECONDS` | `120` | `0..=3600`; `0` disables caching |
| `TODO_MCP_CACHE_MAX_TASKS` | `5000` | `100..=200_000` |
| `TODO_MCP_SYNC_TIMEOUT_MS` | `20000` | `1000..=120_000`; capped by `TODO_MCP_TOOL_DEADLINE_MS` |
| `TODO_MCP_TOOL_RESULT_MAX_BYTES` | `262144` | serialized tool-result cap |

Four rows, not five: `TODO_MCP_MAX_PAGES` (50) belongs to
[`m3-graph-client.md`](m3-graph-client.md) §9 and M4 only *reads* it to derive the request
budget. Constants rather than variables: `BODY_PREVIEW_BYTES` 280 · `BODY_MAX_BYTES` 8192 ·
`BODY_STORE_BYTES` 4096 · `CURSOR_TTL_SECONDS` 900. Deleted: `SYNC_MAX_REQUESTS` (derived),
`BATCH_SIZE` (M3's `BATCH_MAX` constant), `CACHE_PERSIST` and `CACHE_KEY_FILE` (v1 caches
nothing to disk). Every surviving row is a `tests/config_refusals.rs` case, and **no refusal
message ever echoes a value**.

Two spellings are source contradictions, not decisions: the result cap is
`TODO_MCP_MAX_RESULT_BYTES` in one strand of the earlier unpublished research and `TODO_MCP_TOOL_RESULT_MAX_BYTES` in the
other (the latter wins — it cannot be read as M3's `TODO_MCP_MAX_RESPONSE_BYTES`), and the
milestone table writes the sync timeout without the `_MS` every other timeout here carries.
Whichever lands must read identically in `README.md`, `.env.example` and the refusal test.

---

## 9. Tests

| File | Pins |
|---|---|
| `cache_ttl.rs` | second identical search inside the TTL ⇒ fixture request counter **unchanged**; after `ttl+1 s` ⇒ exactly one refetch; `TTL=0` ⇒ every call refetches; catalogue and list expire independently |
| `cache_eviction.rs` | whole-list LRU eviction down to `max_tasks`; the list being served is never evicted; the catalogue is never evicted; `total_tasks` still matches the sum after 1000 randomised insert/evict ops |
| `cache_poison.rs` | panic while the write guard is held ⇒ next request returns correct **empty-cache** results, `mailbox_id` preserved, `generation` bumped, one WARN logged, no underflow |
| `cursor.rs` | base64url round-trip over arbitrary bytes (property test); each of the **7** rejections returns `isError` with the byte-exact string; paging 137 items at `limit=50` yields every id exactly once |
| `sync_budget.rs` | budget exhausted ⇒ `complete:false` + full `coverage`, **never** `isError`; the identical next call converges; priority order is `defaultList` first, `flaggedEmails` last; a partial list keeps its fetched pages |
| `tools_read.rs` | `todo_lists` with `include_counts:true` on a cold cache ⇒ **zero** `/tasks` requests, counts `null`; `todo_account_status` ⇒ **zero** `/me` requests, `account_id == "default"`; the cap ladder per tool; `content[0].text` parses to exactly `structuredContent` on a partial result |

Fixtures: the hand-rolled `tiny_http` Graph server in `tests/common/mod.rs` through the
`test-fixtures`-gated `with_base` hatch ([`m2-mcp-http.md`](m2-mcp-http.md) §9) — no
`wiremock`, no dev-deps. `cache_poison.rs` extends M2's panic contract; the rest are new.

---

## 10. Settle during M4

- **Cold-sync convergence against a real large mailbox.** "Two or three calls" is reasoned,
  not measured; ~40 lists, or a 5000-task *Flagged emails* list, is the case that breaks it.
  If it takes more, raise `TODO_MCP_SYNC_TIMEOUT_MS` or default `todo_agenda` to a `lists`
  allowlist (M5) — never drop lists silently.
- **Default page size — UNVERIFIED.** The superseded plan's "10" has no Microsoft source;
  probe H measures it and `$top=100` makes it moot for correctness. Probe E
  (`$expand=checklistItems`) would make `todo_get_task` one request; v1 stays at two.
- **`mbx_unknown`** — the no-`defaultList` fallback. Untested against a brand-new personal
  MSA that has never opened Microsoft To Do, the one account shape where it happens.
- **The hand-rolled base64url codec** breaks pagination *silently* rather than loudly, which
  is why its round-trip property test is not optional. The ~120-character cursor length is
  computed, not measured: assert an upper bound in `cursor.rs` so a future field cannot push
  it past what a model will copy back intact.

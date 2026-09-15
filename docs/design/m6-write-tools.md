# M6 — Write tools, guards, and scope-derived registration

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Handoff spec for the five mutating tools and the machinery that decides whether they
exist at all. The `Patch<T>` output and the `windows-timezones` failures below were
**compiled and run**; the guard messages are verbatim strings the tests pin. What Graph
accepts on a *write* is still open — this says so rather than guessing. Do not
re-derive any of it.

**Probe J is a blocker for this milestone.** `WindowsTimezone::try_from(Tz)` fails for
`Europe/Kyiv`, `America/Nuuk` and `Asia/Ho_Chi_Minh`, so users in those zones cannot
write a due date at all unless the hand-written alias fallback is accepted by Graph —
unverified (§4). Run probe J *before* writing `graph/tasks.rs`.

**Exit gate.** `tools/list` snapshot is **10** tools under `Tasks.ReadWrite`, **5**
under `Tasks.Read` and **5** unauthenticated · *"add bread, milk, eggs, coffee, rice"*
is **one** `todo_create_tasks` call · `patch_wire.rs` green including the
`Patch::Absent`-serialises-to-`Err` case · `todo_delete_tasks` without `confirm` and
against `flaggedEmails` both fail with **zero** HTTP requests ·
`is_protected("unknownFutureValue") == true` · the 6×3 recurrence table passes.

**Files:** `tools/{write,guards}.rs`, `domain/recurrence.rs`,
`graph/{tasks,checklist}.rs`, `tests/{patch_wire,guards,write_tools}.rs`, and new cases
in M2's `tests/tools_contract.rs`. Budget ~800 src + ~500 test LOC, 3–4 sessions.

---

## 1. The five tools and their annotations

Best-effort per item — there is no transaction on `/me/todo`, so all-or-nothing would be
a lie. Every result splits into success and failure arrays.

| Tool | Batch | RO | D | I | OW | Result arrays, and why the hints |
|---|---|:--:|:--:|:--:|:--:|---|
| `todo_create_tasks` | **1–25** | ✗ | **✗** | **✗** | ✓ | `created[]`, `failed[]`, `warnings[]`. Purely additive; calling twice makes **two** tasks. |
| `todo_update_tasks` | **1–25** | ✗ | **✓** | **✓** | ✓ | `updated[]` (`task_id,title,fields_changed[],cleared[]`), `failed[]`. `clear_*` exists *to destroy*; same args twice = same end state. |
| `todo_complete_tasks` | **1–50** | ✗ | **✗** | **✓** | ✓ | `changed[]`, `unchanged[]` (`task_id,reason`), `failed[]`. Reversible via `completed:false`; nothing destroyed. |
| `todo_delete_tasks` | **1–50** | ✗ | **✓** | **✓** | ✓ | `deleted[]`, `already_absent[]`, `refused[]`, `failed[]`. Permanent, but deleting an already-absent task is a reported no-op (§8). |
| `todo_manage_checklist` | 1 task, ≤20 per verb | ✗ | **✓** | **✗** | ✓ | `added/checked/unchecked/renamed/removed/refused/failed[]` + `checklist_after[]`. `remove` destroys; `add` twice yields two items. |

The five read tools are `true/false/true/true`. Verbatim from
`schema/2025-06-18/schema.ts` (the middle two are documented as *"meaningful only when
`readOnlyHint == false`"*):

| Hint | Spec text, verbatim | Default |
|---|---|:--:|
| `readOnlyHint` | *"If true, the tool does not modify its environment."* | `false` |
| `destructiveHint` | *"If true, the tool may perform destructive updates to its environment. If false, the tool performs only additive updates."* | **`true`** |
| `idempotentHint` | *"If true, calling the tool repeatedly with the same arguments will have no additional effect on the its environment."* | `false` |
| `openWorldHint` | *"If true, this tool may interact with an 'open world' of external entities…"* | **`true`** |

**`openWorldHint` is `true` on all ten tools** — every call reaches
`graph.microsoft.com`, the spec default is true, and the superseded plan had it
backwards. All four hints are written **explicitly**; omitting one is pessimistic, not
neutral. `todo_update_tasks` carrying `destructiveHint: true` is a deliberate UX cost
and the honest reading of *"may perform destructive updates"* given `clear_*`.

> **Annotations are not the enforcement.** The spec's tool-safety text says *"clients
> MUST consider tool annotations to be untrusted"* ([`m2-mcp-http.md`](m2-mcp-http.md)
> §1); they drive consent UX and nothing else. `confirm: true`, the `flaggedEmails`
> refusal and the batch caps are enforced **server-side in `tools/guards.rs`**; the tests
> assert the refusal, never the hint.

Schemas are `additionalProperties: false` with a full `required` list
([`m2-mcp-http.md`](m2-mcp-http.md) §7); `confirm` is `{"type":"boolean","const":true}`
**and** required, so a validating client rejects `false` before the call — most do not
validate, so the server re-checks (**G2**). `todo_create_tasks` takes a top-level `list`
default plus a per-item override, which is what makes the five-groceries call one call;
its inline `checklist` entries are follow-up `POST …/checklistItems` whose failure lands
in `warnings` and **never** fails the task, or the model creates it twice.
`todo_manage_checklist` applies a fixed order — `remove` → `rename` → `uncheck` →
`check` → `add` — and a name matching two items refuses *that op only* with
`ambiguous_name`.

---

## 2. `Patch<T>` — the serde trap

> **`skip_serializing_if = "Option::is_none"` cannot express this, and the failure is
> silent.** That attribute is exactly what makes an omitted field disappear, which makes
> `None`-meaning-*"don't touch"* indistinguishable from `None`-meaning-*"clear this"*.
> Wrong one way and a title-only update silently wipes the user's due date; wrong the
> other and `clear_due_date: true` reports success and changes nothing — both data loss
> the model cannot see. `Option<Option<T>>` does work (`Some(None)` serialises through
> the inner `Option` to `null`) but by accident, and needs a custom `deserialize_with`.

A tri-state whose `Serialize` impl **errors** in the `Absent` state makes a forgotten
`skip_serializing_if` loud, not silent:

```rust
#[derive(Debug, Clone, Default)]
pub enum Patch<T> { #[default] Absent, Null, Set(T) }

// impl Serialize: Null => s.serialize_none(); Set(v) => v.serialize(s);
//   Absent => Err(ser::Error::custom("Patch::Absent must be skipped with
//             skip_serializing_if = \"Patch::is_absent\""))
// TaskPatch: #[serde(rename_all="camelCase")] over title, dueDateTime, startDateTime,
// reminderDateTime, isReminderOn, importance, status, completedDateTime, categories,
// body, recurrence — EVERY field a Patch<_> with skip_serializing_if="Patch::is_absent".
```

A missing attribute now costs a loud `isError`, not a quiet clear. **Verified output** —
run, not reasoned about. With `title: Set("Buy milk")`, `due_date_time: Null`,
`reminder_date_time: Null`, `is_reminder_on: Set(false)`, `categories: Set(vec![])`,
`body: Set({"content":"","contentType":"html"})` and `recurrence: Null`:

```json
{"title":"Buy milk","dueDateTime":null,"reminderDateTime":null,"isReminderOn":false,
 "categories":[],"body":{"content":"","contentType":"html"},"recurrence":null}
```

and `serde_json::to_string(&TaskPatch::default()) == "{}"`.

---

## 3. `clear_*` → wire values

Model-facing **booleans**, never nullables — a nullable argument would push the
tri-state problem into the schema, where the model would have to grasp it.

| `clear_*` flag | Exact JSON emitted | Also emitted | Note |
|---|---|---|---|
| `clear_due_date` | `"dueDateTime": null` | — | **Probe F.** |
| `clear_start_date` | `"startDateTime": null` | — | **Probe F.** |
| `clear_reminder` | `"reminderDateTime": null` | `"isReminderOn": false` | **Paired, always** — see below. |
| `clear_body` | `"body": {"content":"","contentType":"html"}` | — | **Not `null`** — `body` is a complex type; see the callout. |
| `clear_categories` | `"categories": []` | — | Empty array, never `null` — it is a collection. |
| `clear_recurrence` | `"recurrence": null` | — | **Probe G.** Docs are silent. |
| `clear_importance` | `"importance": "normal"` | — | Not a clear at all — the enum has no null state, so this is a *set*, and the earlier unpublished Graph research argues for not offering it. The one row probes F/G cannot touch, and the one whose boolean the raw `todo_update_tasks` schema never declares — add it. |
| `clear_title` | *not offered* | — | `title` is required; a titleless task is not representable. |

> **The reminder pair is one operation, not two.** `"reminderDateTime": null` alone
> leaves `isReminderOn: true`, which the To Do UI renders as a bell with nothing behind
> it. Both go in the same PATCH or the reminder survives its own removal;
> `patch_wire.rs` asserts the pair byte-for-byte, because a refactor that maps fields
> one property at a time is exactly how they come apart.
>
> **`clear_body`'s `contentType` is contradicted across the source material.** The
> condensed plan says `"text"`; the earlier unpublished Graph research says `"html"` and backs it with the
> `Update todoTask` doc's *"only HTML type is supported"* — which is also why an
> outbound `body: "<text>"` goes as `{"content":"<html-escaped>","contentType":"html"}`.
> **Ship `html`.**

Non-flag clears performed by the other tools:

| Situation | Exact JSON |
|---|---|
| `todo_complete_tasks`, `completed: true` | `{"status":"completed"}` — `completedDateTime` is service-assigned and is **never** sent |
| `todo_complete_tasks`, `completed: false` | `{"status":"notStarted","completedDateTime":null}` |
| `body: "text"` | `{"body":{"content":"<html-escaped text>","contentType":"html"}}` |

Reopening lands on `notStarted` even for a task that was `inProgress` or `deferred` —
the prior status is recorded nowhere; say so in the tool description.
**Conflict rule**, refused pre-flight with zero HTTP: *"Update 3 (task `<id>`): due_date
and clear_due_date are mutually exclusive. Pass one or neither."* **If probe F or G
shows `null` is rejected** — a 4xx, or the property simply surviving the PATCH — the
flag does **not** silently no-op; it returns `error_code: "unsupported_clear"`, and
other fields in the patch still apply:

> *"Microsoft Graph does not support clearing `<field>` on a To Do task. The task was
> otherwise updated; `<field>` is unchanged."*

This is the likeliest place in the whole server for a tool to lie to the model, which is
why the failure mode is a named error and not a success with no effect. `patch_wire.rs`
pins every row of both tables (§9).

---

## 4. Outbound dates, and probe J

| Argument | Exact JSON |
|---|---|
| `due_date: "2026-08-25"` | `{"dueDateTime":{"dateTime":"2026-08-25T00:00:00.0000000","timeZone":"<W>"}}` — `start_date` is identical under `startDateTime` |
| `reminder_at: "2026-08-25T09:00"` | `{"reminderDateTime":{"dateTime":"2026-08-25T09:00:00.0000000","timeZone":"<W>"},"isReminderOn":true}` |

`<W>` is owned by [`m5-agenda-timezone.md`](m5-agenda-timezone.md) §8–§9; M6 consumes
it. `prefer_tz_value(tz)` is the Windows name when `WindowsTimezone::try_from(Tz)`
resolves, else a hand-written `ALIAS_FALLBACK` name, else `None`; a bare IANA
`tz.name()` is legal only inside the list the `dateTimeTimeZone` docs enumerate beside
*"any of the time zones currently supported by Windows, as well as the other time zones
supported by the calendar API"* — and **that list was seen but never captured**, so
membership is untestable today. Two results that were **run**, not read. **The round
trip is lossy:** `Europe/Prague` → `"Central Europe Standard Time"` → back through
`chrono_tz` = **`Europe/Budapest`**, so **never round-trip**. **Not every IANA zone
maps:** `try_from` returns `Err(FromChronoTzError)` for `Asia/Kolkata` and `Europe/Kyiv`
(CLDR's primaries are `Asia/Calcutta` and `Europe/Kiev`), and for `Asia/Ho_Chi_Minh` and
`America/Nuuk`; `ALIAS_FALLBACK` answers those four with `India Standard Time`,
`FLE Standard Time`, `SE Asia Standard Time` and `Greenland Standard Time` — every one
hand-written, none CLDR-derived, **none yet seen by Graph**.

> **Why probe J blocks M6 and not M5.** On the *read* path a `None` from
> `prefer_tz_value` is harmless: the timezone half of the `Prefer` header is dropped and
> the client-side conversion takes over, which is correct anyway. On the *write* path
> there is no fallback — the zone string goes into the request body, and if Graph
> rejects it the user cannot set a due date at all. M6 is the first milestone where "is
> this Windows name accepted?" has a wrong answer. Probe J is a single `PATCH` of a date
> with `timeZone: "India Standard Time"` plus a re-`GET`
> ([`m2-mcp-http.md`](m2-mcp-http.md) §10); run it for **each** of the four names above.

A zone in neither set **refuses the write** rather than silently writing UTC midnight and
shifting the user's date by a day. The refusal string is verbatim in
[`m5-agenda-timezone.md`](m5-agenda-timezone.md) §8; the part M6 must not soften is its
`<suggestion>`, which has to name a zone that really maps (`Europe/Kyiv` →
`Europe/Kiev`) and is pinned by a test, never a free-text guess.

---

## 5. Recurrence — pass-through, and what the 6×3 table really tests

`patternedRecurrence` is passed through **verbatim**: `domain/recurrence.rs` checks the
object's shape and the same JSON goes on the wire. No `rrule` crate, no expansion.

> **Correction to the superseded plan.** It justified a strict extra-property rejector
> by claiming Graph errors on unknown recurrence properties. The Microsoft text says
> only that a property with an **unsupported *value*** errors — it says **nothing about
> unknown property *names***. Do not implement, test for, or document extra-property
> rejection as known Graph behaviour. Whether an unknown key inside `recurrence` is
> rejected, ignored or stored is **UNVERIFIED**; it was never probed, and a `PATCH`
> carrying a junk key plus a re-GET would settle it.

So the 6×3 table tests **our** side of the contract, not Graph's: 6 recurrence
**patterns** × 3 recurrence **ranges** = 18 cases, each asserting the object survives
argument → `TaskPatch` → serialized body **unchanged** (key order included, nothing
dropped, renamed or camelCase-mangled by a `rename_all` that should not have applied),
and `is_recurring` derives correctly. **UNVERIFIED and deliberately not restated here:**
the exact `type` enum members of `recurrencePattern` and `recurrenceRange` — the source
material carries no verbatim enumeration, and a wrong member name in a validator would
reject a legal recurrence the user typed correctly. Copy the six and the three from the
Graph resource docs, and keep the natural-language mapping **conservative**.

---

## 6. Guards — pre-flight, zero HTTP

The `todoTaskList.wellknownListName` enum, verbatim: **`none`**, **`defaultList`**
(built-in *Tasks*), **`flaggedEmails`** (built-in *Flagged email*), and
**`unknownFutureValue`** — *"Evolvable enumeration sentinel value. Do not use."*

```rust
/// Fails CLOSED: the enum is evolvable, so `unknownFutureValue` — and any string
/// this build does not recognise — counts as protected. Microsoft can add a
/// built-in list next quarter; failing open silently destroys data.
pub fn is_protected(w: Option<&str>) -> bool { !matches!(w, None | Some("none")) }
```

| # | Guard | Tool | Trigger | Message (verbatim) | HTTP |
|---|---|---|---|---|---|
| **G1** | flagged-email delete | `todo_delete_tasks` | list has `wellknownListName == "flaggedEmails"` | *"Refused: `"<title>"` lives in the built-in Flagged emails list, which mirrors flagged Outlook mail. Deleting it here would not unflag the message and Microsoft To Do does not support it. Unflag the email in Outlook instead."* | ≤ the catalogue GET the delete already needed; **never** per-task |
| **G2** | confirm | `todo_delete_tasks` | `confirm != true` | *"Refused: todo_delete_tasks permanently deletes tasks and requires confirm: true. Nothing was deleted."* | **0** |
| **G3** | checklist-remove confirm | `todo_manage_checklist` | `remove` non-empty, `confirm != true` | *"Refused: removing checklist items is permanent and requires confirm: true. No operations in this call were applied."* | **0** |
| **G4** | batch cap | all write tools | more items than the schema max | *"Refused: todo_`<tool>` accepts at most `<n>` items per call; `<m>` were supplied. Split the call."* | **0** |

A fifth guard for renaming or deleting a built-in list went with **`todo_manage_list`,
cut from v1** — but `is_protected` stays (G1 needs it), with
`tests/guards.rs::{builtin_lists_are_protected, unknown_future_value_is_protected}` so a
v2 tool cannot forget it.

> **"Zero HTTP requests" is exactly true for G2–G4 and conditionally true for G1 — and
> the exit gate asserts both together.** G2, G3 and G4 are pure functions of the
> arguments; they cannot make a request. G1 needs `wellknownListName`, which lives in
> the list catalogue: **zero** requests when it is warm, one — the *same* GET the delete
> needed anyway — when it is cold, never a per-task fetch. So *"delete against
> `flaggedEmails` fails with zero HTTP requests"* is only satisfiable if the test warms
> the catalogue and **then resets the fixture's request counter**. Written the obvious
> way, against a cold cache, it makes a correct implementation fail the gate.

**Explicitly allowed on `flaggedEmails`:** `todo_update_tasks` and `todo_complete_tasks`
— marking a flagged email done is legitimate and reversible, and a blanket refusal would
break the commonest flagged-email workflow. **Only delete is refused.**

---

## 7. Scope-derived registration

Source of truth is the **`scope` field of the token response**, persisted as
`granted_scope` ([`m1-auth.md`](m1-auth.md) §4–§5). **Never a decoded JWT** —
personal-account access tokens are encrypted and will not decode. Locked decision.

```rust
pub enum Grant { ReadWrite, ReadOnly, None }
/// Short name OR last URI segment, case-insensitive (Entra echoes either);
/// ReadWrite checked first because it implies read; `.All` deliberately unmatched.
pub fn grant_from_scope(scope: &str) -> Grant;
```

**Reuse `scope_granted()` and `effective_scope()` from [`m1-auth.md`](m1-auth.md) §4 —
do not re-derive them.** The two traps they exist for, one line each: the `scope` value
is **percent-encoded in some of Microsoft's own samples**, so
`contains("Tasks.ReadWrite")` fails on the encoded form and on case; and the field is
**documented optional**, so an absent `scope` must fall back to the requested scope or
all five write tools silently vanish.

| Tool | `ReadWrite` | `ReadOnly` | `None` |
|---|:--:|:--:|:--:|
| `todo_lists`, `todo_search_tasks`, `todo_agenda`, `todo_get_task`, `todo_account_status` | ✅ | ✅ | ✅ |
| `todo_create_tasks`, `todo_update_tasks`, `todo_complete_tasks`, `todo_delete_tasks`, `todo_manage_checklist` | ✅ | ✗ | ✗ |
| **total in `tools/list`** | **10** | **5** | **5** |

Under `Grant::None` the same five readers are still *listed* — so the model can call
`todo_account_status` and be told how to log in — while the four data readers return
`isError` with the `auth_required` message and the literal shell command. `tools/list`
is computed **once at construction** and cached in `ServerState.tools`, never recomputed
per request — that is precisely how a tool list changes mid-session, and clients cache
them aggressively. Capabilities declare `{"tools": {}}` with **no `listChanged`**,
because `GET /mcp` is 405 and there is no server→client channel to send it on; and no
`cacheScope` hint — no such field exists on `Tool`/`ListToolsResult` anywhere in the legacy
family this server targets, `2025-11-25` and below ([`m2-mcp-http.md`](m2-mcp-http.md) §1). (`2026-07-28` makes it *required*, which is one more reason never to claim that
era — [`m2-mcp-http.md`](m2-mcp-http.md) §1.)

On refresh, `TokenProvider` compares `grant_from_scope(new_scope)` with the running
grant. **Equal** does nothing: refreshes never perturb `tools/list`. **Narrowed**
(`ReadWrite → ReadOnly|None`, consent revoked) and **widened** are both frozen until
restart, and every write handler under a narrowed grant returns `isError`: *"Refused:
the current Microsoft Graph grant is `<grant>`. Write tools require Tasks.ReadWrite.
Re-run login and restart the server."* `todo_account_status` surfaces the divergence as
`scopes_granted` (live) beside `write_tools_enabled` (frozen), plus
`restart_required`/`restart_reason`. The first-run trap this creates is solved
structurally in [`m2-mcp-http.md`](m2-mcp-http.md) §6 — `serve` refuses to start without
a usable token, and the widening transition puts the exact restart command in
`todo_account_status`'s **text**. Do not restate that here.

> **As implemented, `TODO_MCP_SCOPE` is a ceiling.** `auth::vet_grant` caps the granted
> scope at it and refuses any granted `.All` ([`m1-auth.md`](m1-auth.md) §4). Under
> `TODO_MCP_SCOPE=Tasks.Read`, a registration also consented for `Tasks.ReadWrite` gets
> only the five read tools, no restart is suggested, and `login`, `serve` and `doctor` warn
> that the stored refresh token can still write. The refusal quoted above is the
> text under a `Tasks.ReadWrite` config; under `Tasks.Read` the refusal says the server
> is configured read-only and that restarting will not change that.

---

## 8. Write-path semantics

All under `/me` — `/users/{id}/…` exists nowhere and gate 1 enforces it. Creates are
`POST /me/todo/lists/{listId}/tasks`; update, complete and reopen are all
`PATCH …/tasks/{taskId}`; delete is `DELETE …/tasks/{taskId}`; checklist work is
`GET`/`POST` on `…/tasks/{taskId}/checklistItems` and `PATCH`/`DELETE` on
`…/checklistItems/{itemId}`. The client is [`m3-graph-client.md`](m3-graph-client.md).

**Writes bypass the cache** — one request each, occasionally two, neither reading
through it nor populating it. A **successful** mutation invalidates that list's entry
and bumps `generation`; a **5xx or timeout** invalidates it too, because the outcome is
unknown and a confidently-wrong cache is worse than a refetch. Write-through caching is
cut from v1 ([`m4-cache-read-tools.md`](m4-cache-read-tools.md) §2).

**`If-Match` is never sent.** `todoTask` has no documented `etag` property and the
`Update todoTask` header table lists only `Authorization` and `Content-Type`. An
undocumented conditional header is either ignored (false safety, worse than none) or
honoured, making every write against a ≤120 s-stale cache a `412` the model cannot
recover from. The `@odata.etag` real responses carry is captured and exposed on
`todo_get_task`; probe I decides the v2 opt-in.

**Delete tolerates 404 as success**, reporting the id in `already_absent[]` rather than
`failed[]` — that is what makes `idempotentHint: true` on `todo_delete_tasks` honest.
There is **no idempotency-key store in v1**: a repeated `todo_create_tasks` genuinely
creates duplicates, hence its `idempotentHint: false`.

> **The HTTP retry classifier and the MCP idempotency hint disagree about `PATCH`, and
> both are right.** `retryable()` is `429|503|504 => true`, `500|502 =>
> is_idempotent(m)`, and `graph`'s own `is_idempotent(m)` is `matches!(m, Get | Delete)`
> ([`m3-graph-client.md`](m3-graph-client.md) §2, §5) — a free function, deliberately **not**
> `http::Method::is_idempotent()`, which is wider (it takes `PUT`, `HEAD`, `OPTIONS` and `TRACE`
> too) and would quietly widen the retry set if someone reached for the method form. So
> `PATCH` is retried on the first three but **not** on `500`/`502`, where the write may
> already have landed. Meanwhile `todo_update_tasks` advertises `idempotentHint: true`, a
> statement about the *end state the model observes*, not about whether the transport may
> replay a call. `POST` is excluded for the sharper reason: retrying a `POST …/tasks` that
> actually succeeded creates a **duplicate task** the user cannot tell from one they made.
> Never widen `is_idempotent()` for a retry test, and never swap it for the `http` crate's.

Every outbound call takes a permit from the `gate` semaphore — a **field on
`ServerState`**, never a `static`, so `TODO_MCP_GRAPH_CONCURRENCY` can reach it —
**dropped before any `Retry-After` sleep** and acquired *after* the access token
([`m2-mcp-http.md`](m2-mcp-http.md) §5).
Guard refusals, `unsupported_clear`, `auth_required` and throttling are all `isError`
tool results, never JSON-RPC errors ([`m2-mcp-http.md`](m2-mcp-http.md) §7).

---

## 9. Tests

| File | Pins |
|---|---|
| `patch_wire.rs` | one case per `clear_*` row: absent ⇒ property absent; `true` ⇒ byte-for-byte; the reminder **pair**; one per §3 non-flag row (complete, reopen, `body`, outbound dates); `default() == "{}"`; `Patch::Absent` without the skip ⇒ `Err` |
| `guards.rs` | G2 with `confirm:false` **and** with `confirm` absent → refusal, **zero** fixture requests (catalogue pre-warmed, counter reset); G1 on a `flaggedEmails` list → refusal, zero requests; G4 at cap+1 for each write tool; `is_protected("unknownFutureValue") == true`; `is_protected(None) == false`; update and complete on `flaggedEmails` **succeed** |
| `tools_contract.rs` | the `10 / 5 / 5` `tools/list` snapshot over `ReadWrite`/`ReadOnly`/`None` — the test that proves least privilege is **enforced**, not documented; annotations exact on all ten with `openWorldHint: true`; `confirm` is `const:true` and in `required` |
| `recurrence.rs` (unit) | 6 patterns × 3 ranges survive argument → patch → body unchanged; `is_recurring` derivation |
| `write_tools.rs` | a five-item create is **one** tool call producing five `POST`s; one item's failure lands in `failed[]` with the other four `created[]`; a checklist failure lands in `warnings[]` with the task still `created[]`; `DELETE` → 404 lands in `already_absent[]`; `due_date` + `clear_due_date` together → pre-flight refusal, zero requests |
| `scope_grant.rs` (unit) | `grant_from_scope` over real Entra strings: short name, fully-qualified, percent-encoded, mixed case, absent-`scope` fallback, `.All` → `None` |

---

## 10. Settle during M6

- **Probe J — the blocker.** Are the hand-written fallback Windows names accepted in a
  `dueDateTime.timeZone` on write? If not, `Europe/Kyiv`, `Asia/Ho_Chi_Minh` and
  `America/Nuuk` users hit the §4 refusal on every dated write, and the suggested
  substitute had better be right.
- **Probes F and G.** Does `null` actually clear `dueDateTime`, `startDateTime` and
  `recurrence`? Until they run, six of the seven `clear_*` rows are a hypothesis. Record
  the answers in `docs/graph-probe.md` (M2 creates it; all user content redacted per
  [`m8-docs-release.md`](m8-docs-release.md) §6) and wire
  `unsupported_clear` for whichever fail. Add the third null nobody probed: reopen sends
  `"completedDateTime": null`, which F and G do not cover.
- **`clear_body`'s `contentType`** — `html` (cited) versus `text` (uncited); one PATCH
  plus a re-GET settles it, and §3 ships `html`. **Unknown keys inside
  `patternedRecurrence`** — rejected, ignored, or stored? **UNVERIFIED**, never probed;
  the 6×3 table does not test it and must not claim to. **Probe I / `If-Match`**: a
  `412` makes a v2 opt-in defensible, a `200` keeps the door shut.
- **Batch caps 25/25/50/50 are judgement, not measurement.** They bound one tool call's
  request fan-out against the four-concurrent-request ceiling — itself a **conservative
  inference** for `/me/todo/*`, since the published limit is stated against the legacy
  `outlookTask` types. Revisit only with a measured throttle.

# Live-account probe log

**This file is redacted.** It records the shape of what this server sends to
Microsoft Graph and Entra and what comes back, never the content: task titles,
bodies, list names, checklist items, categories, account names and addresses are
replaced by typed placeholders such as `«title»` (the scratch tasks below carried
probe-only titles and are gone); list, task, tenant and client ids are
pseudonymised (`L1`, `L2`, `L-probe`, `T…`) consistently within the file; no access
token, refresh token, device code, real client id or real tenant id appears. Status
lines, header names, timestamps, error codes and response shapes are kept, because
the shape is the point. Nothing here is a raw transcript. The rule is
[m8 §6](design/m8-docs-release.md); the checklist is
[issue #3](https://github.com/hromadkom/microsoft-todo-mcp/issues/3).

One section per checklist item: what was sent, what came back, a one-word verdict,
and the one line of code or docs the verdict changed.

## Run

| | |
|---|---|
| Date | 2026-09-22 |
| Account | one **work or school** account, single-tenant app registration (`TODO_MCP_TENANT` set), `TODO_MCP_SCOPE=Tasks.ReadWrite`, `TODO_MCP_TZ=Europe/Prague` |
| Mailbox | 2 lists: `L1` (`defaultList`, >100 tasks, so paged) and `L2` (`flaggedEmails`, 11 tasks); a scratch list `L-probe` created for the writes and deleted afterwards (`DELETE /me/todo/lists/L-probe` → 204) |
| Server | `docker.io/hromadkom/microsoft-todo-mcp:1.0.1` (the `latest` tag that day) via `compose.yaml`, macOS arm64 Docker Desktop, engine 29.7.2 |
| How | `login` → `doctor` → `up -d` → MCP tool calls over `POST /mcp` with `curl`; the raw Graph requests were sent with `curl` from a throw-away Alpine container that redeemed the stored refresh token for its own access token and printed status lines and body *shapes* only |
| Not done | a **personal** account (none available), and the revoke-consent click paths (no browser session) |

Summary:

| Item | Verdict |
|---|---|
| `$batch` against `/me/todo/*` | **accepted** |
| `Prefer: outlook.timezone` on `/todo` | **applied, but never acknowledged** → code changed |
| `PATCH` with `null` clears dates and recurrence | **clears**, with side effects → docs changed, [#25](https://github.com/hromadkom/microsoft-todo-mcp/issues/25) |
| `ALIAS_FALLBACK` Windows zone names on a dated write | **all accepted** |
| `protocolVersion` per MCP client | **Claude Code `2025-11-25`** |
| Granted `scope` string | **no `.All`; `User.Read` + OpenID trio extra** (work account) |
| Revoke-consent click paths | **not walked** |
| Idle RSS | **measured** (`docs/footprint.md`) |
| `docker compose up -d` reaches `healthy` | **5.5 s** with a real login |
| A real tool call from the musl release image | **works** |
| `$filter` / `$orderby` on `todoTask` (gate 2) | **mixed: partly applied, partly 400, partly ignored** → gate comment changed |
| Recurrence rewrites dates (not on the checklist) | **Graph derives due/start from `range.startDate`** → docs changed, [#24](https://github.com/hromadkom/microsoft-todo-mcp/issues/24) |

## `$batch` against `/me/todo/*`

Sent: `POST https://graph.microsoft.com/v1.0/$batch`, body
`{"requests":[{"id":"1","method":"GET","url":"/me/todo/lists/L1/tasks?$top=100","headers":{"Accept":"application/json","Prefer":"outlook.timezone=\"Central Europe Standard Time\", odata.maxpagesize=100"}}, {"id":"2", … L2 …}]}`.

Came back: `HTTP/1.1 200 OK`, `Content-Type: application/json`, body
`{"responses":[{"id":"2","status":200,"headers":{"Content-Type":…,"OData-Version":"4.0"},"body":{"value":[11 tasks], no nextLink}}, {"id":"1","status":200,…,"body":{"value":[100 tasks],"@odata.nextLink":…}}]}`.
Sub-responses arrive out of order (paired by `id`, as the code does). Sub-response
headers are `Content-Type` and `OData-Version` only. The `Prefer` inside a
sub-request **is honoured**: a second batch with one sub-request carrying
`outlook.timezone` and one without echoed `dueDateTime.timeZone` as
`Central Europe Standard Time` in the first and `UTC` in the second.

Through the server: `todo_lists` cost 2 Graph requests (`/me/todo/lists` then one
`$batch`), `todo_account_status.graph.last_status` 200, and the fallback warning
`$batch rejected; falling back to sequential first pages` never appeared in the
container log.

Verdict: **accepted.** Consequence: none; the sequential fallback stays as the
degradation path.

## `Prefer: outlook.timezone` on `/todo`

Sent: `GET /me/todo/lists/L1/tasks?$top=100` with
`Prefer: outlook.timezone="Central Europe Standard Time", odata.maxpagesize=100`,
then the same with `"UTC"`, with `"Nope Standard Time"`, with
`odata.maxpagesize=100` only, and with no `Prefer` at all.

Came back, every 200 with the same header set:
`HTTP/1.1 200 OK`, `Content-Type: application/json;odata.metadata=minimal;odata.streaming=true;IEEE754Compatible=false;charset=utf-8`,
`OData-Version: 4.0` — **no `Preference-Applied` header in any response**, not
for the timezone half and not for `odata.maxpagesize` either. Yet the preference is
applied: the body's `dueDateTime.timeZone` was `Central Europe Standard Time` for the
first request and `UTC` for every other, with `dateTime` converted accordingly
(`2026-09-29T22:00:00.0000000` UTC ↔ `2026-09-30T00:00:00.0000000` CEST).
`"Nope Standard Time"` → `HTTP/1.1 400 Bad Request`,
`{"error":{"code":"invalidRequest","message":"Invalid request"}}`, so the
400-degradation in `disable_prefer_tz_once` matches what an unknown name does.
`odata.maxpagesize=100` with `$top=100` gave 100 items and a `nextLink`; `$top=2`
gave 2.

Through the server before the change: `doctor` and `todo_account_status` reported
`timezone_mode: client_side`, settled by the first GET (`/me/todo/lists`, which has
no dated field at all).

Verdict: **applied, never acknowledged.** Consequence:
`graph/client.rs::observe` now judges the mode by the first *read* whose body
carries a `timeZone` (a `$batch` first page counts): echoed in the requested zone is
`server_side`, in another zone `client_side`, and a read without a dated field leaves
it open. `tests/graph_client.rs` has the two cases. Dates were resolved client-side
either way, so this changed a reported field, not a date.

## `PATCH` with `null` clears `dueDateTime` / `startDateTime` / `recurrence`

Sent, through `todo_update_tasks` on seven scratch tasks in `L-probe` that each had
a due date, start date, reminder, `importance: high`, one category, a body and a
daily recurrence — one `clear_*` flag per task — then `todo_get_task` on each. The
raw PATCH bodies the tool builds, confirmed by repeating them with `curl` on fresh
tasks:

| Flag | PATCH body | Status | Follow-up GET |
|---|---|---|---|
| `clear_due_date` | `{"dueDateTime":null}` | 200 | `dueDateTime` gone — **and `startDateTime` and `recurrence` gone with it** (reminder kept) |
| `clear_start_date` | `{"startDateTime":null}` | 200 | `startDateTime` gone, due and recurrence kept |
| `clear_reminder` | `{"reminderDateTime":null,"isReminderOn":false}` | 200 | gone |
| `clear_body` | `{"body":{"content":"","contentType":"html"}}` | 200 | `body.content` is `"\r\n"` (Graph's empty HTML body); `body_preview` null, `body_bytes_total` 0 |
| `clear_categories` | `{"categories":[]}` | 200 | `[]` |
| `clear_recurrence` | `{"recurrence":null}` | 200 | gone, due and start kept |
| `clear_importance` | `{"importance":"normal"}` | 200 | `normal` |

`{"startDateTime":null}` on a task that had none is a 200 no-op. Read-after-write
was immediate on every raw probe (GET at +0 s, +1 s, +4 s all showed the change).
One anomaly: the very first `clear_due_date` through the server returned
`cleared: ["due_date"]`, and the `todo_get_task` a second later showed the task
untouched (`modified_at` still equal to `created_at`); the same call two minutes
later cleared it. Whether Graph or the server's cache served that read is not
known.

Verdict: **clears.** Consequence: the `clear_due_date` description and README now
say that Graph removes the start date and recurrence with the due date;
`cleared[]` stays as reported, and the unexplained first no-op plus the side
effects are [#25](https://github.com/hromadkom/microsoft-todo-mcp/issues/25)
(re-read after a clear, or say what was really cleared).

## `ALIAS_FALLBACK` Windows zone names on a dated write

Sent: one `todo_create_tasks` per row, `list: L-probe`, `timezone: <IANA>`,
`tasks: [{title, due_date: "2026-09-30"}]`, which the tool turned into
`"dueDateTime":{"dateTime":"2026-09-30T00:00:00.0000000","timeZone":"<Windows name>"}`:

| `timezone` argument | `timeZone` sent | Status | Stored (read back in UTC) |
|---|---|---|---|
| `UTC` | `UTC` | 201 | `2026-09-30T00:00:00Z` |
| `Etc/UTC` | `UTC` | 201 | `2026-09-30T00:00:00Z` |
| `Etc/GMT` | `UTC` | 201 | `2026-09-30T00:00:00Z` |
| `Asia/Kolkata` | `India Standard Time` | 201 | `2026-09-29T18:30:00Z` |
| `Europe/Kyiv` | `FLE Standard Time` | 201 | `2026-09-29T21:00:00Z` |
| `Asia/Ho_Chi_Minh` | `SE Asia Standard Time` | 201 | `2026-09-29T17:00:00Z` |
| `America/Nuuk` | `Greenland Standard Time` | 201 | `2026-09-30T01:00:00Z` |
| `Europe/Uzhgorod` | `FLE Standard Time` | 201 | `2026-09-29T21:00:00Z` |
| `Europe/Zaporozhye` | `FLE Standard Time` | 201 | `2026-09-29T21:00:00Z` |
| `Europe/Prague` (control) | `Central Europe Standard Time` | 201 | `2026-09-29T22:00:00Z` |

Every stored instant is local midnight of 2026-09-30 in the zone sent, so the
names are accepted *and* interpreted. Note the documented sharp edge: read back in
the server's default zone (Europe/Prague), the Kolkata, Ho Chi Minh and FLE rows
show `due: 2026-09-29`, because midnight there is the previous evening in Prague —
the m5 rule applied as written, not a defect.

Verdict: **all accepted.** Consequence: none. `UTC`, the default configuration,
writes fine.

## `protocolVersion` per MCP client

Captured with a logging proxy between the client and `POST /mcp` that recorded
JSON-RPC method names, the `protocolVersion` fields and the `MCP-Protocol-Version`
header only.

| Client | Sent | Server answered |
|---|---|---|
| `curl` baseline | `initialize` `protocolVersion: "2025-06-18"` | `"2025-06-18"` |
| Claude Code 2.1.278 (`User-Agent: claude-code/2.1.278 (sdk-cli)`) | `"2025-11-25"` | `"2025-11-25"` |

Claude Code's sequence, all `POST /mcp`:

1. `server/discover`, header `MCP-Protocol-Version: 2026-07-28`, before any
   `initialize`. This server answers HTTP 200 with JSON-RPC error `-32601 Method not
   found`. The client tolerates it and continues.
2. `initialize` with `protocolVersion: "2025-11-25"`, no `MCP-Protocol-Version`
   header. Answered `protocolVersion: "2025-11-25"`,
   `serverInfo: { name: "microsoft-todo-mcp", version: "1.0.1" }`.
3. `notifications/initialized`, header `MCP-Protocol-Version: 2025-11-25`, HTTP 202.
4. `tools/list`, header `MCP-Protocol-Version: 2025-11-25`, HTTP 200.

Verdict: **`2025-11-25`.** Consequence: README's Clients table names it. The
`server/discover` pre-flight needs no code; an unknown method is answered exactly as
JSON-RPC says. A tool call issued *by* Claude Code against the signed-in server was
not captured (the headless session stopped at its own permission prompt); the
calls below went through `curl`.

## Granted `scope` string

Sent: the device-code `login`, then a refresh
(`POST https://login.microsoftonline.com/<tenant>/oauth2/v2.0/token`,
`grant_type=refresh_token`, `scope=https://graph.microsoft.com/Tasks.ReadWrite offline_access`).

Came back: `HTTP 200`, keys `access_token, expires_in, ext_expires_in, refresh_token,
scope, token_type`, `expires_in: 3643`, and
`scope: "profile openid email https://graph.microsoft.com/Tasks.ReadWrite https://graph.microsoft.com/User.Read"`
(the token file records it without the resource prefix:
`profile openid email Tasks.ReadWrite User.Read`).

No `.All`. Extras: the portal's default `User.Read` and the OpenID trio, exactly
the case `audit_scope` tolerates: `login` saved the token, `doctor` and `serve`
printed the `Microsoft also granted User.Read` warning, `todo_account_status`
reported `identity: "not requested (no openid scope)"` even though `openid` was
granted — the server never asked for it, so it does not use it.

Verdict: **no `.All`, `User.Read` extra** — for a work account. A clean personal
account is still unrecorded. Consequence: `docs/app-registration.md` § Revoking
consent now states the observed string, so the warning reads as the normal first-run
outcome.

## Revoke-consent click paths

Not walked: this run had no browser session on either account type. The caveat in
`docs/app-registration.md` stays, narrowed to the click paths alone.

Verdict: **pending.** Consequence: none yet.

## Idle RSS

`docker stats --no-stream` on the compose service, signed in: **3.58 MiB** cold
(boot refresh done, no request), **4.19 MiB** after `todo_lists` (catalogue,
`$batch`, TLS session, two lists cached), **7.96 MiB** after about sixty tool calls
including the writes and a 42-task search. Follow mode without a token measured
6.16 MiB cold and 7.92 MiB after a handshake in an earlier start; the cold number
moves by a few MiB between starts.

Verdict: **measured.** Consequence: `docs/footprint.md` § Idle RSS has the table;
the 96m cap has an order of magnitude of headroom.

## `docker compose up -d todo-mcp` reaches `healthy`

With a real `token.json`, `docker compose up -d todo-mcp` reported
`Up 5 seconds (healthy)`; polled every 0.5 s, `healthy` appeared **5.5 s** after
`up -d` returned, i.e. at the healthcheck's `start_period: 5s` floor — the boot
refresh finished well inside it. Follow mode without a token: 5.6 s.

Verdict: **healthy.** Consequence: none.

## A real tool call from the musl release image

Sent, to the pulled `1.0.1` image: `initialize`, then `todo_lists`
(`include_counts: true`), then `todo_account_status` (`check_connectivity: true`),
then the whole write-probe sequence above — roughly sixty `tools/call`s, each one
a rustls handshake or a reused session on a 512 KiB worker thread.

Came back: `todo_lists` listed both lists with `coverage: null` (complete);
`todo_account_status.connectivity.ok: true`, `graph.requests: 2`,
`graph.last_status: 200`, `graph.transport_failures: 0`; no panic, no worker
restart, nothing above `warn` in the log besides the `User.Read` notice.

Verdict: **works.** Consequence: `WORKER_STACK` stays at 512 KiB.

## `$filter` / `$orderby` on `todoTask`

Not on issue #3's list, but gate 2 in `scripts/gates.sh` cites this file for it.
Sent by hand against `L1` (100 tasks on the first page unfiltered):

| Query | Status | Result |
|---|---|---|
| `$filter=status eq 'notStarted'` | 200 | 7 items, no nextLink — **applied** |
| `$filter=status eq 'completed'` | 200 | 100 items + nextLink — applied |
| `$filter=importance eq 'high'` | 200 | 7 items — applied |
| `$orderby=createdDateTime desc` | 200 | 100 items, verified descending — **applied** |
| `$orderby=dueDateTime/dateTime` | 200 | 100 items, undated tasks first — applied |
| `$orderby=title` | **400** `invalidRequest` "Invalid request" | rejected |
| `$search="x"` | 200 | 100 items, identical to unfiltered — **silently ignored** |

Verdict: **mixed.** Graph applies some `$filter`/`$orderby` on `todoTask`, hard-errors
on others and ignores `$search`; nothing documents which. Consequence: gate 2's
comment now says so; all filtering, sorting and searching stays client-side.

## Recurrence rewrites the dates (found on the way)

Sent, raw, to `L-probe`: `POST …/tasks` with `dueDateTime` 2026-09-30 in
`Central Europe Standard Time` and a `recurrence` whose `range.startDate` was
2026-09-25 or 2026-09-30, in daily and weekly patterns, in CEST, UTC and India
Standard Time; then `PATCH dueDateTime` on a recurring task.

Came back (all 201/200):

| Sent | Stored due | Stored `range.startDate` |
|---|---|---|
| due 09-30 CEST, no recurrence (control) | 09-30 | — |
| due 09-30 CEST + daily from 09-25 | **09-26** | **09-26** |
| due 09-30 CEST + daily from 09-30 | **10-01** | **10-01** |
| due 09-30 CEST + daily every 2 from 09-26 | **09-28** | **09-28** |
| due 09-30 IST + daily from 09-30 | **10-01** | **10-01** |
| due 09-30 **UTC** + daily from 09-25 | 09-25 | 09-25 |
| due 09-30 **UTC** + daily from 09-30 | 09-30 | 09-30 |
| due 09-30 CEST + weekly (Wed) from 09-30 | 09-30 | 09-30 |
| due 09-30 CEST + weekly (Fri) from 09-25 | 09-25 | 09-25 |
| daily from 09-25, no due sent | 09-25 | 09-25 |
| due 09-30 + start 09-25 CEST + daily from 09-30 | **10-06** | start **10-01** |
| PATCH due 10-05 CEST on a daily task due 09-30 | **10-01** | unchanged |
| PATCH due 10-07 UTC on that task | **10-02** | unchanged |

So with a recurrence Graph ignores the `dueDateTime` sent and sets it from
`range.startDate`; a daily pattern with a non-UTC zone lands one interval *after*
`startDate` (and moves `startDate` with it); a start date keeps its distance from the
due date; and a `PATCH` of the due date on a recurring task advances it by exactly one
occurrence whatever value is sent. The server reports the truth already, because
`todo_create_tasks` echoes Graph's response (the created task showed
`due: 2026-10-01` for a 2026-09-30 request) — but nothing warns the caller.

Verdict: **Graph-owned.** Consequence: the `recurrence` description and README say
that Graph derives the dates; [#24](https://github.com/hromadkom/microsoft-todo-mcp/issues/24)
asks whether the tools should send a UTC-zoned due for recurring tasks or refuse
`due_date` next to `recurrence`.

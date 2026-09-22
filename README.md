# microsoft-todo-mcp

[![CI](https://github.com/hromadkom/microsoft-todo-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/hromadkom/microsoft-todo-mcp/actions/workflows/ci.yml)
[![Docker Image](https://img.shields.io/docker/v/hromadkom/microsoft-todo-mcp?label=docker&sort=semver)](https://hub.docker.com/r/hromadkom/microsoft-todo-mcp)
[![Image Size](https://img.shields.io/docker/image-size/hromadkom/microsoft-todo-mcp/latest)](https://hub.docker.com/r/hromadkom/microsoft-todo-mcp)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An [MCP](https://modelcontextprotocol.io) server that exposes **only Microsoft
To Do** — your task lists, over Streamable HTTP, on one delegated Graph scope and
nothing else.

> ### 🚧 Status: alpha — tested offline, never run against a live Microsoft account
>
> All ten tools and the six subcommands are implemented. What has been verified, and
> where:
>
> | | |
> |---|---|
> | **Test suite** | 221 tests (115 unit, 106 integration) against a hand-rolled fixture Entra/Graph server. Green under the host time zone and under `TZ=Pacific/Kiritimati`, and inside `docker build --target test .` |
> | **Release image** | Built for `linux/amd64` and `linux/arm64` through the size gate, and checked on macOS arm64 Docker Desktop: user `65534:65534`, `/data` volume, healthcheck, no shell. A compose run with a fake client ID showed a fresh named volume's `/data` owned `65534:65534` mode `0700` (Docker Desktop keeps named volumes on ext4 inside its Linux VM; only bind mounts go through VirtioFS), and the `Restarting (3)` refusal without a sign-in |
> | **Linux** | CI's `Release image` job asserts that first-mount ownership again on the amd64 image on a native Linux engine, and is the only evidence for `docker stop` during boot exiting 0 within 2 s (as PID 1, no init). Stopping a running server, and its drain, are tested on the host by `tests/shutdown.rs` |
> | **Never done** | A run against a real Microsoft account. The live-Graph assumptions — `$batch` on `/me/todo/*`, whether `null` clears a due date, which Windows zone names a date write accepts, the exact scopes Microsoft grants — are still assumptions. [CHANGELOG.md](CHANGELOG.md) lists them all |

---

## Why this exists

A general-purpose "Microsoft 365 MCP" server asks for a scope surface covering
mail, files and calendar. This one asks for exactly `Tasks.ReadWrite` +
`offline_access`.

The interesting part is that the broad alternative is not merely broader — **for
writes it does not exist**. App-only `Tasks.ReadWrite.All` is documented
`"Not supported."` for *every* Microsoft To Do write operation; only the reads and
the two delta functions carry an application permission at all. Anything that
writes your tasks *must* use a delegated token. The only real question is what
else that token can reach. Here: nothing.

That is enforced structurally, not editorially:

| Property | How it's enforced |
|---|---|
| One delegated scope | Requests exactly `Tasks.ReadWrite` (or `Tasks.Read`) + `offline_access`; `TODO_MCP_SCOPE` accepts nothing else. If the scope Microsoft reports back includes any `.All` permission, the token is refused: `login` exits 1 and saves nothing, `serve` will not start and any later refresh is refused, and `doctor` reports a finding. Any other Graph permission granted on top (typically the portal's default `User.Read`) is named in a warning and never used; `openid`, `profile`, `email` and `offline_access` are ignored. |
| No cross-user access | The `/users/{id}/…` path shape **exists nowhere in the codebase**. Only `/me`. A build gate enforces it. |
| No secret, no daemon identity | Public client, no client secret. `TODO_MCP_CLIENT_SECRET` being set is a startup refusal. |
| Tool surface follows the *actual* grant in default mode, capped by your config in follow mode | In default mode, write tools are registered only if the scope Microsoft reports in the token response (or, if it omits one, the scope requested) is `Tasks.ReadWrite` **and** `TODO_MCP_SCOPE` is `Tasks.ReadWrite`. With `TODO_MCP_START_WITHOUT_TOKEN=1`, `tools/list` is built from the `TODO_MCP_SCOPE` ceiling and dispatch enforces the live grant; a later widening or narrowing needs no restart. |
| Never asks for your identity | `openid`, `profile` and `User.Read` are not requested, and the server never calls the profile endpoint `GET /me`: every Graph request it makes is under `/me/todo/`, directly or inside `$batch`. |
| No task content in the server's logs | No line the server logs carries a task title, body, category, checklist item or list name. A list appears in logs only as an opaque `lst_…` hash (`cache::log_ref`), and a test re-runs its own binary to check the real stderr. [SECURITY.md](SECURITY.md#known-gaps) lists the known gaps. |
| Task content never touches disk | The cache is memory-only. The volume holds `token.json` (the refresh token) and `bearer.token` (the MCP bearer), never your tasks. |

The refresh token is stored **in plaintext** at `/data/token.json`, mode `0600`, in a
`0700` directory owned by uid 65534, next to the MCP bearer in `bearer.token`. At-rest
encryption was considered and cut from v1 — protect the volume, and treat both files
like passwords.

## Tools

Ten. Reads collapse into one universal search rather than four near-identical tools a
model has to disambiguate; writes are **plural by default**, so "add these five things
to my shopping list" is one call. The five write tools exist only under a
`Tasks.ReadWrite` grant.

| Tool | Purpose |
|---|---|
| `todo_lists` | Lists with their ids and, when the cache is warm, open/overdue counts. Never fetches tasks. |
| `todo_search_tasks` | The universal read: one list or all, status, due window, importance, category, free text; sorted, cursor-paginated. |
| `todo_agenda` | The "plan my day" primitive — overdue / due today / due soon / no due date / flagged emails / recently completed in **one** call. |
| `todo_get_task` | Full fidelity for one task incl. checklist, linked resources and attachment *metadata* (never bytes). |
| `todo_account_status` | Grant, granted scopes, whether write tools are enabled, token expiry, time zone, cache and Graph counters. No identity, no token material. |
| `todo_create_tasks` | Create 1–25 with body, dates, reminder, importance, categories, checklist (up to 20 items each), recurrence. |
| `todo_update_tasks` | Update 1–25. Clearing uses explicit `clear_*` booleans, never nullables. |
| `todo_complete_tasks` | Complete/reopen 1–50. |
| `todo_delete_tasks` | Delete 1–50, permanently. Requires `confirm: true`. Refuses `flaggedEmails`. |
| `todo_manage_checklist` | Declarative add/check/uncheck/rename/remove on one task, up to 20 of each, in one call. `remove` requires `confirm: true`. |

**Write limits.** The batch sizes above are what one call accepts, not what it is
guaranteed to finish. Before the first change, every task id is matched to its list
once: from `list` if you pass it, else from the cache (at any age), else by a single
all-lists sync. After that, each task, checklist item or checklist change is one Graph
request (plus retries), sent in order. Reads count against `TODO_MCP_MAX_PAGES` — the
list catalogue, the id lookup, and the checklist reads of `todo_manage_checklist` and
`todo_get_task` — but the changes themselves do not: they are bounded by
`TODO_MCP_TOOL_DEADLINE_MS`. An item that did not happen is reported in `failed[]`
with an `error_code`:

- `deadline` — the tool deadline elapsed before the item was attempted; nothing was
  sent for it. A newly created task's checklist items that were not reached go to
  `warnings[]` instead.
- `sync_incomplete` (`todo_update_tasks`, `todo_complete_tasks` and
  `todo_delete_tasks` only) — the id lookup stopped before it reached the id, so
  nothing changed. Pass `list`, or call again (which resumes the lookup only when the
  cache is enabled).
- `list_not_found` (the same three tools only) — the `list` you passed does not name
  exactly one list, or a *complete* sync of every list did not contain the id. It is
  never used for a lookup that ran out of budget.

`todo_create_tasks` and `todo_manage_checklist` have no per-item form of those last
two. `todo_create_tasks` refuses the whole call (`isError`, nothing sent) if a
top-level or per-item `list` does not resolve. `todo_manage_checklist` does the same
if its `list` does not resolve, its task lookup stops early, a complete sync lacks the
id, or the checklist itself cannot be read completely. `deadline` still applies per
item to both.

A maximal checklist batch will not finish against real Graph: 25 tasks × 20 checklist
items is 525 requests, and `todo_manage_checklist` with 5 verbs × 20 is 100, against a
25 s default deadline and an assumed (not yet measured) 150–400 ms per request. Expect
`deadline` for the tail, and split large batches. Sustained large batches may also be
throttled by Graph, reported per item as `throttled`. The same code covers any retry
that could not fit before the deadline: the backoff after a Graph server error or a
network failure, or the wait for one of the `TODO_MCP_GRAPH_CONCURRENCY` request slots
while other calls hold them all.

All filtering, sorting and searching happens **client-side**. Whether Graph honours
`$filter` on `todoTask`, ignores it or hard-errors has not been probed live, and
server-side filtering risks being confidently wrong rather than visibly broken.

Deliberately cut from v1: moving tasks between lists (Graph has no move endpoint —
it would be create+delete, changing the task id), list create/rename/delete,
attachment writes.

## Quick start

**Prerequisites:** Docker Engine 23+ (BuildKit) and Docker Compose v2.24+
(`docker compose version`); older Compose rejects the shipped `compose.yaml`'s
optional `env_file`. Images are published for `linux/amd64` and `linux/arm64`.

You also need an app registration; the server ships no client ID. That is a
one-time ~10 minute job: **[docs/app-registration.md](docs/app-registration.md)**.

```bash
cp .env.example .env            # set TODO_MCP_CLIENT_ID; uncomment TODO_MCP_TZ with your zone
docker compose pull todo-mcp

# 1. one-time sign-in (prints a URL and a code; finish in a browser)
docker compose run --rm todo-mcp login

# 2. sanity check: config, token state, granted scopes, your lists, a due-date sample
#    (it prints your list names and can print a task title: redact before sharing)
docker compose run --rm todo-mcp doctor

# 3. run it
docker compose up -d todo-mcp

# 4. point Claude Code at it (-T is required — see below)
claude mcp add --transport http todo http://127.0.0.1:8591/mcp \
  --header "Authorization: Bearer $(docker compose run --rm -T todo-mcp token)"
```

Then ask: *"what's on my task lists?"* or *"plan my day"*. (`docker compose build
todo-mcp` instead builds the same image from your checkout.)

The order matters. `serve` refuses to start until `login` has written a token (exit 3,
`AUTH_REQUIRED`), and `restart: unless-stopped` turns any startup refusal into a loop
that `docker compose ps` shows as `Restarting (N)`, N being the
[exit code](#exit-codes): 3 means not signed in, 2 invalid configuration such as a
missing `TODO_MCP_CLIENT_ID`, 1 a sign-in or token that Microsoft or the server refused.
`docker compose logs --tail 20 todo-mcp` names the fix. After `login`, run
`docker compose restart todo-mcp`. In the default mode the tool list is fixed at
startup, so a grant widened later needs that restart. For a sidecar that should
wait on the listener before sign-in, set `TODO_MCP_START_WITHOUT_TOKEN=1`; it can
use Compose's `condition: service_healthy`. This follow mode uses the configured
scope ceiling, notices later login, logout, dead-token deletion, and account
switches on the next tool call, resets token state and task cache, and follows a
changed live grant without a restart. A `login` file (`obtained_by: "device_code"`)
or a changed scope resets state; a refresh rotation that names its source via
`rotated_from` is adopted silently. It reports `auth_required`, `auth_failed`,
or the transport error until the missing token, Microsoft refusal, or Entra
outage is fixed.

```yaml
services:
  sidecar:
    depends_on:
      todo-mcp:
        condition: service_healthy
```

**`docker compose down -v` deletes your sign-in and MCP bearer** (the `todo-mcp-state`
volume holds both); use `docker compose down`.

> The `-T` on `docker compose run` is **required** wherever output is captured. With a
> TTY, the container's stderr is merged into the captured stdout, so a log line — such
> as the one `token` logs when it first generates the bearer — would end up inside the
> `Authorization` header. (`token` prints no trailing newline, and the server trims
> whitespace around the bearer, so the TTY's `\r\n` rewrite is not the problem.) If
> `token` fails, the substitution is empty and every request gets a 401:
> `docker compose run --rm -T todo-mcp token | wc -c` should print 64.

Without Docker, a host toolchain works the same way. `cargo run --release -- <command>`
is the same as `todo-mcp <command>`, the form every hint the server prints uses
(`cargo build --release` puts the binary at `target/release/todo-mcp`). Point
`TODO_MCP_DATA_DIR` at a directory you own, and bind loopback: the default
`0.0.0.0:8591` listens on every interface, outside a container too.

```bash
export TODO_MCP_CLIENT_ID=<your-app-id> TODO_MCP_TZ=Europe/Prague \
       TODO_MCP_DATA_DIR="$HOME/.todo-mcp" TODO_MCP_BIND=127.0.0.1:8591
cargo run --release -- login
cargo run --release -- doctor
cargo run --release -- serve
cargo run --release -- token      # from another shell with the same variables: the client's bearer
```

## Clients

This server is **HTTP-only**: Streamable HTTP, `POST /mcp` (there is no SSE stream,
and `GET /mcp` is a 405). There is no stdio transport, deliberately — a single
long-lived process is what gives single-writer custody of the token store and a cache
that survives across client sessions.

It negotiates MCP protocol versions `2025-11-25`, `2025-06-18`, `2025-03-26` and
`2024-11-05`; a client asking for any other version is answered with `2025-11-25`.
Which version each client actually negotiates has not been recorded yet.

| Client | Status |
|---|---|
| **Claude Code** | ⏳ Expected to work — the legacy `initialize` handshake it falls back to is what this server speaks; not yet run against this server |
| **Claude Desktop → Code tab** | ⏳ Expected to work (it *is* Claude Code); not yet run |
| **Claude Desktop → chat** | ❌ **Not supported.** Its config schema requires `command`; `url`/`headers` entries are silently stripped. Remote connectors run from Anthropic's cloud and cannot reach localhost. |
| **Hermes Agent** | ⏳ Untested |

If a client can only speak stdio, the answer is a user-run bridge you install, not
a transport in this codebase:

```bash
export AUTH_HEADER="Bearer $(docker compose run --rm -T todo-mcp token)"
npx -y mcp-remote http://127.0.0.1:8591/mcp --allow-http --transport http-only \
  --header "Authorization:${AUTH_HEADER}"
```

`AUTH_HEADER` holds the whole header value, `Bearer ` prefix included; the server
rejects a bare token with a 401. The `--allow-http` and `--transport http-only` flags
have not been verified against mcp-remote's own documentation.

## Configuration

**Configuration is environment-only** (env > default): no command-line flag overrides
a variable, and there is no config file. A value that is empty or only whitespace
counts as unset. Every secret is a **file path**, never an inline value — env leaks
via `docker inspect`, `/proc/1/environ` and image history.

Ranges are inclusive. **A value out of range is a startup refusal, never clamped**,
and every problem found is reported at once.

| Variable | Default | Accepted | Notes |
|---|---|---|---|
| `TODO_MCP_CLIENT_ID` | — | a GUID | **Required** for `serve` and `login`; `doctor` reports it missing as a finding. The Application (client) ID of your app registration. |
| `TODO_MCP_CLIENT_SECRET` | — | must be unset | Any value is a startup refusal: this is a public client and never sends a secret. |
| `TODO_MCP_TZ` | `UTC` (with a warning) | an IANA name, case-insensitive | "Overdue" and "due today" are undefined without it. Set it to the zone Outlook → Settings → Language and time shows. Any IANA name works for reading, but a **dated write** needs a zone Graph can map to a Windows time-zone name: `doctor` shows it as "Windows name for writes", and a write in a zone without one is refused with a nearby zone suggested. Tools also take a per-call `timezone` argument. |
| `TODO_MCP_SCOPE` | `Tasks.ReadWrite` | `Tasks.ReadWrite` or `Tasks.Read` | A **ceiling**: with `Tasks.Read` the five write tools stay absent even if Microsoft grants `Tasks.ReadWrite`. The stored refresh token is read-only only if the app registration grants just `Tasks.Read`; otherwise `login`, `serve` and `doctor` warn that it can write your tasks. Changing it requires a new `login`. |
| `TODO_MCP_START_WITHOUT_TOKEN` | `0` | `1`/`0` (also `true`/`false`/`yes`/`no`) | With `1`, the mode is follow mode whether or not the boot refresh succeeds: `/healthz` is 200, the tool list is the `TODO_MCP_SCOPE` ceiling, and dispatch enforces the live grant. Every tool call stats `token.json` (mtime, length and inode); a changed or vanished file from login, logout, dead-token deletion, or account switching resets token state and task cache, while a same-chain refresh rotation whose `rotated_from` names the prior file is adopted silently. Missing tokens answer `auth_required`, Microsoft refusals answer `auth_failed`, and unreachable Entra returns the transport error until fixed. Refresh failures retry at most every 30 seconds for the same `token.json` but never hide a still-valid access token, including after a non-deleting Entra refusal; a new `login` retries immediately. `todo_account_status` with `check_connectivity:false` touches no network. An unusable `token.json`, refused grant, configuration, data-directory or bind failure still exits. |
| `TODO_MCP_TENANT` | `common` | a tenant GUID, a verified domain, `organizations` or `consumers` | For a single-tenant app registration (AADSTS50194). |
| `TODO_MCP_BIND` | `0.0.0.0:8591` | `<ip>:<port>` | Listens on every interface, outside a container too. In the container the compose port mapping (`127.0.0.1:8591`) decides reachability; on a host run set `127.0.0.1:8591`. |
| `TODO_MCP_DATA_DIR` | `/data` | an absolute path | Created `0700` if missing; a looser mode is a warning. Holds `token.json` (0600), `bearer.token` (0600 when generated), `.token.lock` (0600, the lock every read and write of `token.json` takes) and short-lived `token.json.tmp.*` files. |
| `TODO_MCP_BEARER_FILE` | `<data dir>/bearer.token` | a path | The inbound MCP bearer. Generated (256-bit, hex) on the first `token` or `serve`; a file you supply is read as-is, and its mode is not checked. |
| `TODO_MCP_ALLOWED_HOSTS` | — | hostnames, comma-separated | Extra `Host` header values accepted on `/mcp` (any port). `localhost`, `127.0.0.1` and `[::1]` are always accepted, and the bind address only when it is a specific IP — with the default `0.0.0.0` bind, a LAN address, a compose service name or a reverse proxy's hostname must be listed here. |
| `TODO_MCP_GRAPH_CONCURRENCY` | `4` | `1..=4` | Concurrent Graph requests. 4 is a conservative inference: Microsoft publishes four concurrent requests per mailbox for Outlook resources, and its resource table lists To Do only under the legacy `outlookTask` types, not `todoTask`. |
| `TODO_MCP_MAX_PAGES` | `50` | `1..=10000` | Graph **read** requests per tool call: list-catalogue pages, `$batch` requests, task pages, `@odata.nextLink` follows and single task/checklist/attachment reads all count, retries included. A page holds 100 tasks, so at the default a list of more than 5 000 tasks never completes (it stays partial in `coverage`). The changes the write tools make do not count; see [Tools](#tools). |
| `TODO_MCP_MAX_ATTEMPTS` | `4` | `1..=8` | Attempts per request: 1 try + 3 retries on 429/503/504 (500/502 only for GET and DELETE). |
| `TODO_MCP_HTTP_TIMEOUT_MS` | `20000` | `1000..=120000` | Per outbound request, Entra and Graph. |
| `TODO_MCP_TOOL_DEADLINE_MS` | `25000` | `1000..=120000` | Wall-clock ceiling on one tool call. Write items not attempted before it are reported with `error_code: deadline`. |
| `TODO_MCP_SYNC_TIMEOUT_MS` | `20000` | `1000..=120000`, capped at `TODO_MCP_TOOL_DEADLINE_MS` | Ceiling on one all-lists sync. A partial result carries a `coverage` block; with the cache enabled, the next call reuses the lists already synced. |
| `TODO_MCP_MAX_RESPONSE_BYTES` | `8388608` | `65536..=67108864` | Cap on one Graph response body. |
| `TODO_MCP_CACHE_TTL_SECONDS` | `120` | `0..=3600` | Memory-only task cache. `0` disables it: no task or list data is kept between tool calls, every call re-reads what it needs from Graph, and `todo_lists` counts are always null. Cursors keep working. |
| `TODO_MCP_CACHE_MAX_TASKS` | `5000` | `100..=200000` | Above this, whole lists are evicted, least recently fetched first (a read does not count), never the list just stored — so one list larger than the cap is still kept. A tool result holds everything its own call fetched either way. |
| `TODO_MCP_TOOL_RESULT_MAX_BYTES` | `262144` | `16384..=4194304` | Serialized tool-result cap; results shrink in defined steps before erroring. |

A configuration refusal names the variable and what it accepts but never echoes the
value you set, so an ID or a secret pasted into the wrong variable does not end up in a
log (asserted by `refusals_never_echo_the_value` in `src/config.rs`). Two deliberate
exceptions: a file-system error prints the data-directory or bearer-file path it could
not use, and an unrecognised command-line argument is named back to you.

`todo-mcp doctor` prints the resolved configuration, the `/data` and token state, the
granted scopes read back from Microsoft (with a warning for any this server does not
use), **your list names and, when `TODO_MCP_TZ` is set, one real task's title** with
its raw due date beside the interpreted local date. That last line is the only way to
notice a *valid but wrong* `TODO_MCP_TZ`. It names the fix for anything it detects,
exits 1 when it found something, and never refuses to run (an invalid configuration is
reported as a finding and ends the report). Under `docker compose run`, the service's `json-file` log driver
records that output on the Docker host until the `--rm` container is removed; to avoid
it, run `doctor` from a host build or with `docker run --rm --log-driver none`
([SECURITY.md](SECURITY.md#doctor-and-login-print-to-stdout-and-docker-may-keep-it)
has the command). **Redact the list names and any task title before pasting `doctor`
output anywhere.**

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. `doctor`: no findings. `healthcheck`: healthy. `serve`: stopped by SIGINT/SIGTERM, including during startup or by a second signal; requests still running when the 5 s drain ends (or at a second signal) are abandoned, and a drain-deadline abandonment is logged as a warning. |
| `1` | A runtime error: Microsoft refused the sign-in, Graph or the network failed, `token.json` is unusable or its grant is refused (a `.All` permission, or no Tasks permission), the port could not be bound, or the signal handlers could not be installed. With `TODO_MCP_START_WITHOUT_TOKEN=1`, a sign-in Microsoft refused or an unreachable Entra keeps `serve` up instead; an unusable `token.json` or a refused grant still exits. `doctor`: at least one finding, invalid configuration included. `healthcheck`: unhealthy for any reason, invalid configuration included — Docker reads `1` as unhealthy and reserves `2`. |
| `2` | Usage error (an unknown command or flag) or invalid configuration, including a data directory or bearer file that cannot be used. |
| `3` | Not signed in: `serve` found no `token.json` (logged as `AUTH_REQUIRED`). Run `login`. With `TODO_MCP_START_WITHOUT_TOKEN=1`, `serve` stays up instead and tool calls answer `auth_required` until `login` has run. |
| `130` / `143` | A one-shot command (`login`, `logout`, `token`, `doctor`, `healthcheck`) was interrupted by SIGINT / SIGTERM. `serve` never exits with these. |

### Signing out and rotating the MCP bearer

The data directory holds two unrelated credentials, and `logout` touches only one:

- **`logout` deletes the Microsoft sign-in**: `token.json` and any stale
  `token.json.tmp.*`, under the `.token.lock` lock a refresh also takes (the lock file
  itself stays). It does not revoke the refresh token at Microsoft: remove
  the app at <https://account.microsoft.com/privacy/app-access> for a personal account,
  or see [Revoking consent](docs/app-registration.md#revoking-consent) for a work or
  school account.
- **It keeps `bearer.token`**, the inbound MCP bearer your clients send, and says so
  when that file exists. That is a local credential with nothing to do with your
  Microsoft account.

A running `serve` notices a changed or vanished `token.json` on the next tool call.
Login and logout replacements reset its token state and task cache, while a same-chain
refresh rotation (including one written by `doctor`) whose `rotated_from` names
the prior file is adopted silently without discarding the valid access token or
warm task cache. It no longer uses a signed-out
account.
Its next refresh finds no `token.json` and never writes one back, even if that refresh
was already under way when you ran `logout`, and it will not start again (exit 3)
until the next `login`, unless `TODO_MCP_START_WITHOUT_TOKEN=1`.

```bash
docker compose run --rm todo-mcp logout
docker compose stop todo-mcp
```

To rotate the MCP bearer, delete `bearer.token`, restart `serve` (it reads the bearer
once at startup and generates a new one if the file is missing), then run `token` again
and update the `Authorization` header in every client. The old bearer keeps working
until that restart.

```bash
# with the shipped compose.yaml (the image has no shell, so borrow busybox for the rm)
docker run --rm -v microsoft-todo-mcp_todo-mcp-state:/data busybox rm /data/bearer.token
docker compose restart todo-mcp
docker compose run --rm -T todo-mcp token

# host toolchain
rm "$TODO_MCP_DATA_DIR/bearer.token"   # or wherever TODO_MCP_BEARER_FILE points
# restart `serve`, then:
cargo run --release -- token
```

## Architecture

One crate, one static musl binary, no async runtime.

```
src/
├─ main.rs, cli/  the six subcommands; cli/out.rs is the only writer to stdout
├─ mcp.rs         hand-rolled JSON-RPC 2.0 / MCP — no SDK
├─ http.rs        tiny_http; POST /mcp, GET /healthz; bearer + Origin/Host guards; the drain
├─ server.rs      ToolProvider on &self; the list sync; per-concern locks, no global mutex
├─ auth/          device-code flow and refresh over ureq — no oauth2 crate; the scope policy
├─ graph/         the only module that names a Graph path; paging, retry, $batch
├─ cache.rs       the memory-only TTL cache
├─ domain/        datetime (the only caller of from_local_datetime), filter, resolve, recurrence
└─ tools/         the ten tools, their schemas and guards
```

The outbound `Authorization` header is set only in `graph/client.rs`, and `http.rs`
reads the inbound one; a build gate allows exactly those two files.

The dependency graph is **69 packages** (`grep -c '^\[\[package\]\]' Cargo.lock`,
which counts the root crate, so 68 dependencies). The obvious alternative — the `rmcp`
SDK plus axum, tokio, reqwest and oauth2 — resolved to **243** by the same measure, in
a scratch manifest measured 2026-08-26 that is not in the repo, because rmcp pulls
chrono, schemars, futures and indexmap non-optionally, and pairing it with `oauth2`
forces *five* duplicated majors that no feature flag removes: `reqwest` 0.12 **and**
0.13, `rand` 0.8 **and** 0.10, `base64` 0.22 **and** 0.23, `getrandom` 0.2 **and** 0.4,
`thiserror` 1.0 **and** 2.0. For a project whose whole point is a small resident
footprint, that was backwards. [docs/footprint.md](docs/footprint.md) has the measured
binary and image sizes.

Accepted cost: protocol conformance is ours to maintain.

`chrono` is built **without** the `clock` feature, which makes `chrono::Local`
structurally unreachable rather than merely discouraged — the host timezone cannot
leak into a due-date calculation even by accident.

See [AGENTS.md](AGENTS.md) for the invariants worth knowing before editing, and
[docs/design/](docs/design/README.md) for the historical pre-implementation specs that
code comments cite as `(mN §k)`.

## Development

Dockerized, so no host Rust toolchain is assumed:

```bash
docker compose run --rm dev cargo test --locked
docker compose run --rm -e TZ=Pacific/Kiritimati dev cargo test --locked  # host-TZ independence
docker compose run --rm dev cargo clippy --all-targets --locked -- -D warnings
docker compose run --rm dev cargo fmt --check
docker compose run --rm dev sh scripts/gates.sh
docker build --target test .        # the hermetic gate: fmt, clippy, tests, gates
```

`docker build --target test .` runs formatting, lints, the test suite and the gates,
but **not** the `TZ=Pacific/Kiritimati` re-run: that is a separate step, above
locally and in CI inside the image the gate built.

A host toolchain also works and iterates faster — `rust-toolchain.toml` pins the
version. Note your machine's default `stable` may be older and unable to compile
this crate at all (edition 2024; `File::lock` needs 1.89).

`scripts/gates.sh` holds ten structural invariants that a type system cannot
express — no `/users/` path, no `$filter` under `src/graph/`, the `Authorization`
header named in exactly two files, exactly one TLS stack, and so on. Each corresponds
to a claim this README or [SECURITY.md](SECURITY.md) makes, so a failing gate means
the documentation became untrue.

CI runs two required checks on every pull request and every push to `main`:
**`Hermetic gate`** (the build above plus the time-zone re-run) and **`Release image`**
(both architectures through the binary size gate, then a Linux smoke test of the amd64
image). Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## Releasing

Releases are driven by GitHub Releases — there is nothing to run locally:

1. Bump `version` in `Cargo.toml`, refresh `Cargo.lock` (`cargo check`, or
   `docker compose run --rm dev cargo check`), and move the `[Unreleased]` entries in
   [CHANGELOG.md](CHANGELOG.md) under the new version. Commit.
2. Create a GitHub Release with tag `vX.Y.Z`. The `v` prefix is required.

[`.github/workflows/release.yml`](.github/workflows/release.yml) then refuses a tag
that is not `vX.Y.Z` (a SemVer pre-release suffix is allowed) or that disagrees with
`Cargo.toml`, runs the hermetic gate, and pushes multi-arch `linux/amd64` +
`linux/arm64` images to `docker.io/hromadkom/microsoft-todo-mcp` tagged `X.Y.Z`,
`sha-<short-sha>` and `latest`. A release marked as a pre-release, or a pre-release
version such as `v0.2.0-rc.1`, publishes the version tags but leaves `:latest` alone.

To re-run a release, for example after fixing registry credentials:
`gh workflow run release.yml -f tag=vX.Y.Z -f latest=true`. A re-run moves `:latest`
only with `-f latest=true`, so leave it off when rebuilding an older tag.

Forks publish to their own namespace by setting the repository variables `IMAGE_NAME`
and `REGISTRY`, plus the `DOCKERHUB_USERNAME` / `DOCKERHUB_TOKEN` secrets. Versions
follow [Semantic Versioning](https://semver.org/).

## Security

**Can:** read and write the To Do lists of the single account that signed in.

**Cannot:** read anyone else's tasks; read your mail, files, calendar or contacts;
ask for your identity; act without you; or persist task content to disk.

Two design notes worth stating plainly:

- **Token separation is absolute.** The inbound MCP bearer guards `/mcp` and is
  never forwarded to Graph; a Graph token is never accepted as MCP auth. This is
  the most commonly violated rule in Graph-backed MCP servers, so it is structural
  here: the Graph token only ever comes from the token store, which knows nothing
  about inbound HTTP.
- **The static bearer is not "spec-legal".** MCP authorization is *optional*, and
  the specification is **silent** on static bearer tokens — they are out of scope,
  not blessed. This server does not claim MCP-conformant authorization; doing so
  would put it under a SHOULD for OAuth 2.1 and a MUST for RFC 9728 protected
  resource metadata, neither of which v1 ships. Bind to loopback, and put TLS in
  front if the port is reachable off-host.

The full model, the `/data` credential inventory and the known gaps are in
[SECURITY.md](SECURITY.md). **Do not open a public issue for a security problem**;
report it through
[private vulnerability reporting](https://github.com/hromadkom/microsoft-todo-mcp/security/advisories/new).

## License

MIT — see [LICENSE](LICENSE).

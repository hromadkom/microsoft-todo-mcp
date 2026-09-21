# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Opt-in startup without a sign-in.** `TODO_MCP_START_WITHOUT_TOKEN=1` keeps
  `serve` healthy for sidecar deployments when no token exists, Microsoft refuses
  the sign-in, or Entra is unreachable. The tool list uses the configured ceiling,
  dispatch follows the live grant through a lock-free atomic, and a later wider
  login needs no restart. Corrupt token files and refused grants still refuse startup.

## [0.1.0] - 2026-09-15

First public release.

### Added

- **Sign-in and operator commands.** `login` signs you in with Microsoft's device-code
  flow: it prints a URL and a code, you finish in a browser, and it needs no TTY.
  `doctor` reports the resolved configuration, the token and `/data` state, the granted
  scopes and, with `TODO_MCP_TZ` set, a due-date sample, and names the fix for anything
  it finds. `token` prints
  the MCP bearer your clients send. `logout` deletes the Microsoft sign-in, keeps the
  MCP bearer, and does not revoke anything at Microsoft. `healthcheck` probes a
  running server and exits 0 when healthy, 1 otherwise.
- **Ten MCP tools.** Five read tools are always listed: `todo_lists`,
  `todo_search_tasks`, `todo_agenda` ("plan my day" in one call), `todo_get_task` and
  `todo_account_status`. Five write tools, `todo_create_tasks`, `todo_update_tasks`,
  `todo_complete_tasks`, `todo_delete_tasks` and `todo_manage_checklist`, are
  registered only under `Tasks.ReadWrite`. Writes are batched: create and update take
  1–25 tasks (a created task may carry up to 20 checklist items), complete and delete
  take 1–50, and `todo_manage_checklist` takes up to 20 items per verb. Each item
  succeeds or fails on its own. `todo_delete_tasks` requires `confirm: true` and
  refuses the `flaggedEmails` list.
- **`TODO_MCP_SCOPE` is a ceiling.** With `Tasks.Read`, the write tools stay out of
  `tools/list` even if your app registration was also consented for `Tasks.ReadWrite`;
  `login`, `serve` and `doctor` then warn that the stored refresh token can still write.
- **`.All` permissions are refused.** If the scope Microsoft reports includes any
  organisation-wide `.All` permission, `login` saves nothing, `serve` refuses to start,
  and `doctor` reports it. Any other extra Graph permission Microsoft grants, typically
  `User.Read`, is named in a warning and never used.
- **Streamable HTTP transport.** It serves `POST /mcp` and an unauthenticated
  `GET /healthz`, and negotiates MCP protocol versions `2025-11-25`, `2025-06-18`,
  `2025-03-26` and `2024-11-05`. Every request to `/mcp` needs the bearer, which is
  generated on first use as `bearer.token`. `Origin` and `Host` checks against DNS
  rebinding and a 1 MiB request-body cap follow the bearer check.
- **Memory-only task cache.** It has a TTL (`TODO_MCP_CACHE_TTL_SECONDS`, default 120 s;
  `0` keeps nothing between calls) and whole-list eviction above
  `TODO_MCP_CACHE_MAX_TASKS`. Task content is never written to disk. The server's own
  log lines name a list only by an opaque hash and carry no task titles; `SECURITY.md`
  lists the known gaps. A result that could not read every list says so in a
  `coverage` block.
- **Results do not depend on the host time zone.** "Overdue" and "due today" follow
  `TODO_MCP_TZ`, which defaults to UTC with a warning. The host's `TZ` and
  `/etc/localtime` are never read, and days on which local midnight does not exist are
  handled.
- **Container image.** A `FROM scratch` image holding one static musl binary, for
  `linux/amd64` and `linux/arm64`, published as `docker.io/hromadkom/microsoft-todo-mcp`.
  It runs as uid 65534, has no shell, and keeps its state in a `/data` volume with
  mode `0700`.
- **Hardened `compose.yaml`.** The port is published on loopback only
  (`127.0.0.1:8591`), with a read-only root filesystem, all capabilities dropped,
  `no-new-privileges`, swap disabled and a built-in healthcheck.
- **Predictable shutdown.** `serve` exits 0 on SIGINT or SIGTERM, including when the
  signal arrives during startup. Requests already running get up to 5 s to finish.
  Anything still running after that is abandoned and logged as a warning, and a second
  signal exits at once.
- **Documented exit codes and sign-in errors.** Exit codes are 0, 1 (an error, or
  unhealthy for `healthcheck`), 2 (usage or configuration), 3 (not signed in), and
  130/143 when a one-shot command is interrupted. Named remediations cover common
  Microsoft sign-in errors (AADSTS codes), matching the troubleshooting table in
  `docs/app-registration.md`.

### Known limitations

- **Never run against a live Microsoft account.** All testing so far is offline,
  against a fixture Entra/Graph server, and on the release image without a sign-in.
  Still unverified:
  - whether Graph accepts `$batch` on `/me/todo/*` (the server falls back to sequential
    requests if it does not);
  - whether Graph honours `Prefer: outlook.timezone` on To Do;
  - whether a PATCH with `null` actually clears a due date, start date or recurrence
    (`todo_update_tasks` reports `cleared` from the 2xx, without re-reading);
  - whether Graph accepts the hand-written fallback Windows time-zone names
    (`ALIAS_FALLBACK`) on a date write;
  - which `protocolVersion` each MCP client negotiates;
  - the exact scope strings Microsoft grants to personal and work accounts;
  - the server's idle memory use;
  - a real tool call from the musl release image, which would be the first TLS
    handshake on a 512 KiB worker thread.
- **Large checklist batches can outrun the deadline.** Every task and every checklist
  item is a separate Graph request, sent in order. A large `todo_create_tasks` or
  `todo_manage_checklist` call can therefore take longer than
  `TODO_MCP_TOOL_DEADLINE_MS` (25 s by default). A task or checklist change not reached
  in time is reported in `failed[]` with `error_code: deadline`; a checklist item of a
  newly created task that was not reached is reported in `warnings[]`.

[Unreleased]: https://github.com/hromadkom/microsoft-todo-mcp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/hromadkom/microsoft-todo-mcp/releases/tag/v0.1.0

# AGENTS.md

Guidance for AI coding agents working in this repository. If your agent reads
`CLAUDE.md` or `.cursorrules`, treat this file as the source of truth.

## Project

An MCP server exposing **only Microsoft To Do**, over Streamable HTTP, in a small
container. One delegated Graph scope (`Tasks.ReadWrite` or `Tasks.Read`, plus
`offline_access`) and nothing else. The public
[calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp) is the architectural
template — match its conventions unless this file says otherwise.

Public repository (MIT, `github.com/hromadkom/microsoft-todo-mcp`). `CONTRIBUTING.md`
is the contributor-facing version of this file; `SECURITY.md` holds the threat model
and the `/data` credential inventory.

`docs/design/m1…m8` are the historical pre-implementation specs. Code comments cite
them as `(mN §k)` (milestone N, section k). Where a spec and the code disagree, the
code, README.md and SECURITY.md win; the deliberate departures are listed under
Status below.

## Common commands (all dockerized — no host Rust toolchain assumed)

```bash
docker compose run --rm dev cargo test --locked
docker compose run --rm -e TZ=Pacific/Kiritimati dev cargo test --locked   # host-TZ independence
docker compose run --rm dev cargo clippy --all-targets --locked -- -D warnings
docker compose run --rm dev cargo fmt --check
docker compose run --rm dev sh scripts/gates.sh
docker build --target test .            # the hermetic gate: fmt + clippy + test + gates (not the TZ re-run)
docker build --target size --no-cache-filter size --progress=plain .   # size gate, host arch, prints the bytes
docker build --target release .         # the release image, through the size gate
```

A host toolchain also works and is faster to iterate on: `rust-toolchain.toml` pins
1.97.1. Note the machine's default `stable` may be far older and **cannot** compile
this crate (edition 2024, and `File::lock` needs 1.89). The host checkpoint is
`cargo fmt && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked && TZ=Pacific/Kiritimati cargo test --locked && sh scripts/gates.sh`.

## Architecture

```
main → cli ─┬→ http → mcp                  transport; knows nothing of Graph or auth
            ├→ server ⇄ tools              server.rs is the ToolProvider; tools/ read ServerState
            │     └→ cache → domain → graph::models
            │     └→ graph → auth → clock
            └→ auth, config, graph         login, token, doctor
graph → sem (the Graph permit)     config → errors
leaves, importing no crate module: clock, errors, logger, mcp, sem
```

- `mcp.rs` — hand-rolled JSON-RPC 2.0 / MCP. **No SDK.** `handle_message(&self)`.
- `http.rs` — tiny_http, `POST /mcp` + `GET /healthz`, 4 workers on 512 KiB stacks,
  guards (bearer → `Origin` → `Host` → body size), the 5 s drain.
- `server.rs` — `ToolProvider` impl on `&self`; mutability lives in per-concern locks.
  Owns the list sync (`catalogue`, `sync_lists`).
- `auth/` — device-code flow and refresh hand-rolled over ureq. No `oauth2` crate.
  `vet_grant` is the scope policy, `store.rs` the flock'd token file, `aadsts.rs` the
  sign-in error remediations.
- `graph/` — the only place a Graph path is named. `client.rs` sets the outbound
  `Authorization` header; `paging.rs` origin-checks `@odata.nextLink`.
- `cache.rs` — the memory-only TTL cache, and `log_ref`.
- `domain/datetime.rs` — the only caller of `from_local_datetime`.

## Conventions and gotchas

- **stdout belongs to `cli/out.rs`; stderr belongs to `logger.rs`.** `clippy.toml`
  bans `println!`/`eprintln!` elsewhere and `scripts/gates.sh` double-checks it.
  This is not tidiness: `token` is captured by shell substitution into an
  `Authorization` header, so a stray write corrupts a bearer. Never `eprintln!` —
  it panics on a closed pipe and would kill a worker mid-request.
- **Two credentials, never conflated.** The inbound MCP bearer guards `/mcp`; a
  separate expiring OAuth token guards outbound Graph. The bearer is never
  forwarded to Graph and a Graph token is never accepted as MCP auth.
- **The `Authorization` header is named in exactly two files.** `graph/client.rs`
  sets the outbound one and `http.rs` reads the inbound one; gate 3 allows nothing
  else, so do not name it in a `src/` test either.
- **`@odata.nextLink` is followed verbatim but origin-checked first**
  (`graph/paging.rs::assert_graph_origin`, also on `$batch` sub-responses). It is a
  server-controlled URL and we attach a bearer to it. Scheme must be `https`, host
  must be `graph.microsoft.com`.
- **`/users/{id}/…` exists nowhere.** Only `/me`. Gate 1 enforces it.
- **All filtering, sorting and searching is client-side.** Whether Graph silently
  ignores `$filter` on `todoTask` or hard-errors is **UNVERIFIED; no live probe is
  recorded yet (see Status)**. Client-side is the conservative choice either way.
- **`chrono` is built without `clock`.** That makes `chrono::Local` structurally
  unreachable (E0433), not merely lint-banned, and keeps `iana-time-zone` out of
  the tree (gate 6). Wall-clock reads go through the injected `Clock`. The only
  `#[allow(clippy::disallowed_methods)]` sites are `clock.rs` (`SystemClock::now`,
  the injected clock itself), `logger.rs::now_iso` (log timestamps),
  `auth/store.rs::nanos` (tmp-file name uniqueness) and `cli/commands.rs::chrono_now`
  (`login`'s compare-and-swap key). Do not add another.
- **`TODO_MCP_MAX_PAGES` counts reads only.** Write tools resolve every id to its list
  once (`tools/write.rs::resolve_targets`: the `list` argument, then the cache at any
  age, then at most one `sync_lists`), then mutate on `Budget::for_writes`
  (mutations × (`TODO_MCP_MAX_ATTEMPTS` + 1)). Never sync inside a mutation loop:
  every mutation invalidates its list, so a per-item re-sync re-fetches the whole list
  per id, spends the read budget, and takes the `refill` lock (no timeout, held across
  HTTP) once per item. A lookup cut short is `sync_incomplete`, never `list_not_found`.
- **A sync result is built from what the call holds, never by re-reading the cache**
  (`server.rs` `fresh_entries` / `keep` / `outcome`). At
  `TODO_MCP_CACHE_TTL_SECONDS=0` the cache stores nothing, and at any TTL `put_list`
  may evict a list the same sync just fetched (`TODO_MCP_CACHE_MAX_TASKS`).
  `ServerState::sync_lists(lists, budget)` syncs against a catalogue the caller holds.
- **One scope policy: `auth::vet_grant`.** `login` (through
  `cli::commands::finish_login`) and `TokenProvider::access_token` both call it before
  anything is saved; `serve`, `doctor` and `todo_account_status` read its result
  through `TokenProvider`. It refuses any granted `.All`, refuses a grant with no
  Tasks permission, and caps the rest at `TODO_MCP_SCOPE` — a ceiling, because Entra
  returns every scope already consented, not just the one requested. Never decide on
  `grant_from_scope` alone. `finish_login` is split out of `login` so
  `tests/token_store.rs` can test it: `EntraClient::new` is https-only and gate 4
  forbids `with_base`.
- **No user content in a log line.** stderr is persisted by the compose `json-file`
  driver. No `logger::` call may carry a task title, body, category, checklist item,
  list name or a Graph `error.message`; name a list with `cache::log_ref("lst", &id)`.
  The test is the re-exec pair in `tests/tools_read.rs`
  (`a_failed_batch_sub_request_logs_neither_the_list_name_nor_its_id` /
  `child_sync_with_a_failing_sub_request`). `doctor` printing list names and (with
  `TODO_MCP_TZ` set) a task title to stdout is by design, which is why every doc tells
  users to redact it.
- **Sign-in and restart advice comes from `errors::LOGIN_HINT` and
  `errors::RESTART_HINT`.** Both name the host form (`todo-mcp login`) and the compose
  form. They are used by `AppError::NotLoggedIn`, the `TOKEN_STORE` texts
  (`AppError::from_auth` and `store::StoreError`, which `serve` logs at boot), the
  extra-scope warning and the `.All` refusal (`auth::ScopeAudit::warning`,
  `forbidden_scope_message`), the `auth_required` tool text, the write-tool refusal
  under a narrower grant, `todo_account_status.login_command`, `doctor` and `logout`
  (which also shares `auth::REVOKE_CONSENT_PATHS` with the `.All` refusal). Known
  exceptions still say a bare `login`: the AADSTS remediations in `auth/aadsts.rs`
  (`&'static str` built with `concat!`, which cannot take a `const`; a refresh failure
  logs them too), the expired-device-code text in `auth/device_code.rs` and
  `finish_login`'s refusals (both printed by `login` itself), and the write refusal
  under `TODO_MCP_SCOPE=Tasks.Read`, which describes the operator's steps. Do not add
  another.
- **The AADSTS table is tied to code.** `docs/app-registration.md#troubleshooting`
  must list exactly the 20 codes in `auth/aadsts.rs` `KNOWN_CODES`, which must equal
  the `by_code` match arms; unit tests `include_str!` both the doc and `aadsts.rs`.
  Adding a code means editing `by_code`, `KNOWN_CODES` and the table together. A
  non-AADSTS row's bold first cell must not start with a bare number. The Dockerfile
  test stage copies the doc, and `.dockerignore` re-includes it.
- **Apart from an explicit `logout` (`store::delete`), only a refresh deletes
  `token.json`**: only on AADSTS 530036, 70008 or 700082, and only through
  `store::delete_if_unchanged`, a compare-and-delete on `obtained_at` under
  `.token.lock`, so a `login` that lands meanwhile survives and is not redeemed again.
  `logout` deletes under `.token.lock` too, and a refresh's `save_atomic` only replaces
  a `token.json` it can read (a missing one returns `NotFound`; a corrupt or
  future-schema one is refused), so a `logout` during a refresh sticks
  (`tests/token_store.rs::a_logout_during_a_running_refresh_is_not_undone`). Neither
  deleter unlinks `.token.lock` — removing a held flock file breaks mutual exclusion.
  `login` never deletes, and never claims to.
- **Exit codes are a container contract** (`cli::exit`, listed in `USAGE`, tested,
  and mirrored in README's Exit codes table). `healthcheck` returns 1 for every
  failure, invalid configuration included (Docker reserves 2). One-shot commands exit
  130/143 on SIGINT/SIGTERM through `exit_on_interrupt`, which also makes them
  interruptible as PID 1; `serve` exits 0 on a signal.
- **Locks are not uniformly poison-tolerant.** The token gate and the semaphore
  absorb poison (`unwrap_or_else(|e| e.into_inner())`) because their state has no
  cross-field invariant. The **cache resets instead** — a panic mid-write can
  desync `total_tasks` from `tasks`.
- **The Graph permit must be dropped before sleeping on `Retry-After`.** Holding
  four permits through one 120 s backoff parks every other tool call.
- **`panic = "unwind"`**, diverging from calendar-ics-mcp's `abort`: this is a
  long-lived multi-client server. It only pays for itself together with the panic
  hook in `main.rs`, `catch_unwind` around the **whole worker loop** (not just the
  handler body), and the poison policy above.
- **`-T` on every `docker compose run` whose output is captured.** Compose allocates a
  TTY by default, and a TTY merges the container's stderr into the captured stdout, so
  a log line (such as the one `token` logs when it first generates the bearer) lands
  in the header. The `\r\n` rewrite is not the issue: `token` prints no trailing
  newline, and `http.rs` trims the bearer.
- Domain errors return `{ isError: true }` tool results, never JSON-RPC errors and
  never panics across the boundary. `clippy::unwrap_used` warns in **both** crate
  roots — `lib.rs` and `main.rs` are separate and the lint does not carry across.

## Tests

Three layers, no `wiremock` and no dev-dependencies: `tests/common/mod.rs`
hand-rolls a fixture Entra/Graph server on `tiny_http` (already a dependency), so
`cargo test --locked` resolves the exact release graph.

1. In-module unit tests with injected `GraphTransport` / `Clock` stubs.
2. Fixture-driven integration tests in `tests/`. `harness_booted(extra, granted_scope)`
   boots through the real `TokenProvider`, the way `serve` does.
3. Real-subprocess smoke tests over `CARGO_BIN_EXE_todo-mcp` with `env_clear()`. Where
   the code under test cannot run offline or in-process, a test re-executes **its own
   test binary** as a child with `env_clear()` and an env flag:
   `tests/shutdown.rs` (`child_process_entry`, `TODO_MCP_SHUTDOWN_TEST_CHILD`),
   because a booted `serve` needs a live Entra; and the log-hygiene pair in
   `tests/tools_read.rs` (`TODO_MCP_LOG_HYGIENE_CHILD`), because libtest does not
   capture `logger`'s writes to fd 2. Neither can pass vacuously on a filter typo: the
   log-hygiene parent asserts the child's stdout says `1 passed`, and the shutdown
   tests wait for the child's own log lines before signalling it.

**208 tests per run: 113 unit** (in the lib; `main.rs` has none) **and 95
integration** — cli_smoke 10, graph_client 10, http_auth 2, shutdown 8 (one is the
`child_process_entry` body, a no-op outside the child), token_store 15, tools_read 26,
tools_write 24 — and 0 doctests. The count is the same under the host zone, under
`TZ=Pacific/Kiritimati` and inside `docker build --target test .`.

Rules:

- Never call `Shutdown::install()` or `exit_on_interrupt()` in-process in a test: a
  real handler would `_exit(0)` the test runner on Ctrl-C and report a cancelled run
  as a success. Use a re-exec child, or `Shutdown::install_with` with a fake
  registrar. `cli/commands.rs` has exactly one `#[cfg(test)] mod tests`.
- No `unwrap_err()` on non-`Debug` types; use `let … else`.
- No `"Authorization"` or `with_ymd_and_hms` in a `src/` test: gates 3 and 10 scan
  `src/`. `tests/` is fine.
- Fixture data is fake: no real titles, list names, tenant or client IDs, or tokens.
  Assertion messages print the result they judged.

`scripts/gates.sh` holds **ten** structural invariants. Each corresponds to a claim
the README or SECURITY.md makes, so a failing gate means the docs became untrue. The
gates strip **full-line** comments before matching (a doc comment describing an
invariant would otherwise trip the gate enforcing it) but never trailing `//`,
which would eat the tail of a URL literal and turn a violation into a silent pass.

## CI

`.github/workflows/ci.yml` runs on every pull request against `main`, every push to
`main` and on manual dispatch. Two jobs, no secrets and no registry writes. Their names
are the required-check names, exactly:

- **`Hermetic gate`** — the composite action `.github/actions/hermetic-gate`:
  `docker build --target test .` for linux/amd64, then `cargo test --locked --offline`
  again inside that image under `TZ=Pacific/Kiritimati`.
- **`Release image`** — builds `--target release` for linux/amd64 + arm64 without
  pushing. `release` copies its binary from `size`, so this is the size gate, at the
  Dockerfile's `ARG SIZE_LIMIT` default (no build-arg is passed). It then loads the
  amd64 image and smoke-tests it on Linux, as PID 1 with no init: `--version` matches
  Cargo.toml; the image config (user, `/data` volume, entrypoint, `serve`, port,
  healthcheck, no `TODO_MCP_CLIENT_ID` in Env); `--entrypoint sh` fails; the first
  mount of a fresh named volume gives a root of `65534:65534 700`, a `bearer.token` of
  `65534:65534 600` and a 64-hex `token`; `serve` without a token exits 3 with
  `AUTH_REQUIRED`; with a FIFO `token.json` parking `serve` in the boot refresh,
  `docker exec -u 0 … sh` fails and `docker stop` exits 0 in under 2 s; `healthcheck`
  with nothing listening exits 1 with empty stderr (a refused configuration also exits
  1, but logs an error line); `docker compose config -q` passes.

Both jobs time out at 90 minutes. The FIFO step waits for the log text
`TODO_MCP_TZ is unset` (`TZ_WARNING` in `cli/commands.rs`), which `serve` logs before
the boot refresh: keep that prefix and that order. A red smoke step means the code or
the image is wrong — fix it, or remove the claim the step backs; never loosen the
assertion.

The gate lives in the composite action and is called by both `ci.yml` and
`release.yml`, so the two cannot disagree about what the gate is. It is composite
rather than a `workflow_call` reusable workflow deliberately: a called workflow's job
reports as `<caller job> / <called job>`, which would rename the required checks.
Callers do their own `actions/checkout`, so `release.yml` can check out the tag.

`main` is the **sole producer** of the BuildKit layer cache (`type=gha`, scopes `test`
and `release`, shared by both workflows). A pull request's cache is readable by that
pull request alone, and a release runs on a tag ref whose cache neither `main` nor pull
requests can read; both would write entries nothing can restore, so they only consume,
via `cache-from`. Pull-request runs cancel a superseded run; `main` and manual runs are
never cancelled. BuildKit `--mount=type=cache` mounts are *not* carried by `type=gha`,
so a runner's cargo registry starts cold every time.

Dependabot's `github-actions` ecosystem scans `.github/workflows` and a *root*
`action.yml` only, so `.github/dependabot.yml` lists `/.github/actions/hermetic-gate`
explicitly; add a directory entry alongside any new composite action. It cannot see
the `dev` service's `dockerfile_inline` `rust:<version>-slim` pin — keep that in sync
with `rust-toolchain.toml` by hand.

The `Protected main` ruleset (a pull request with zero approvals, strict required
checks `Hermetic gate` and `Release image`, no deletion or force-push, an always-bypass
for admins) is created **after** the first release, so the amend and force-push path
stays open until v0.1.0 is out. From then on, renaming either job leaves `main`
unmergeable against a check that never reports; update the ruleset in the same change.

## Releasing

Publishing is GitHub-Actions-only; there are no release scripts in the repo. Bump
`version` in Cargo.toml, refresh Cargo.lock (`cargo check`), move `[Unreleased]` under
the new version in `CHANGELOG.md` (Keep a Changelog; `release.yml` only warns if the
section is missing), commit, then create a GitHub Release tagged `vX.Y.Z`.
`.github/workflows/release.yml` then:

- **Verifies.** The tag must match `^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$` — the
  `v` prefix is required — and its version must equal Cargo.toml's (read with
  `grep -m1 '^version = '`, so `[package]`'s version must stay the first such line).
  The tag reaches the shell through `env: TAG`, never a `${{ }}` pasted into `run:`:
  git allows `$`, `(` and `"` in ref names. Then the hermetic gate runs, reading the
  cache only.
- **Publishes** `docker.io/hromadkom/microsoft-todo-mcp:{<version>, sha-<7>, latest}`
  for linux/amd64 + arm64, with provenance `mode=max`, no SBOM and no QEMU: the builder
  is pinned to `$BUILDPLATFORM` and cargo-zigbuild cross-compiles both musl targets
  natively — keep it that way, emulation is roughly an order of magnitude slower.
  `sha-<7>` and the OCI revision label come from the tag's commit (`git rev-parse HEAD`
  in the verify job), not metadata-action's `type=sha`, which uses `github.sha` — the
  dispatching branch's tip on a manual run. `publish` checks out that verified SHA,
  not the tag again, so a tag moved between the two jobs cannot ship untested code
  under the tested revision label. `:latest` is pushed only for a GitHub
  release not marked pre-release, or a manual run with `-f latest=true`; a SemVer
  pre-release version (`v0.2.0-rc.1`) never gets it. There is no `cache-to`: `main`
  is the sole cache producer.

Re-run, e.g. after fixing registry credentials:
`gh workflow run release.yml -f tag=vX.Y.Z -f latest=true`. The tag must already
exist and contain `.github/actions/hermetic-gate`; leave off `-f latest=true` when
rebuilding an older tag. Repo variables `IMAGE_NAME`/`REGISTRY` and secrets
`DOCKERHUB_USERNAME`/`DOCKERHUB_TOKEN` let a fork publish elsewhere.

## Docs

- `docs/app-registration.md` — **v1-blocking.** The server ships no client ID, so
  nobody but the author can start the container without it. Written; the click
  paths are unverified against a live portal. Its Troubleshooting table is tied to
  `aadsts.rs` (see Conventions).
- `docs/footprint.md` — the measured binary and image sizes, ELF type, the
  `SIZE_LIMIT` rule and the commands behind them.
- `docs/design/` — the historical specs (see Project).
- `docs/graph-probe.md` — does not exist yet; the live run creates it (see Status).

Two facts in `docs/app-registration.md` were verified against Learn and are worth not
re-deriving: Microsoft's managed consent policy (**the default for a new tenant**)
excludes `Tasks.Read`/`Tasks.ReadWrite`/`Tasks.*.Shared`/`People.Read` from end-user
consent — so work/school needs an admin. And the "Allow public client flows"
manifest key is `allowPublicClient` in the Azure AD Graph manifest format,
`isFallbackPublicClient` in the Microsoft Graph format; both default to false,
which is what makes AADSTS7000218 the top first-run failure.

**Do not put Graph permission GUIDs in docs.** Three sources gave three different
values for `Tasks.ReadWrite`, all sharing a prefix and diverging in the last
segment — the signature of a fabricated tail. Users tick a checkbox by name.

**Probe-output redaction.** Anything recorded from a live account — `docs/graph-probe.md`,
an issue, a fixture — redacts **all** user content (task titles, bodies, list names,
checklist items, categories, account names and emails), replaces ids with consistent
pseudonyms, and is never a raw transcript. `doctor` output counts: it prints list names
and can print a task title.

## Status and where to start

**Implemented and tested offline.** `auth/`, `graph/`, `domain/`, `cache.rs`,
`tools/`, `server.rs`, `mcp.rs`, `http.rs` and the six subcommands exist. The 208
tests (see Tests) pass under the host zone, under `TZ=Pacific/Kiritimati` and inside
`docker build --target test .`, with clippy `-D warnings`, `cargo fmt --check` and
the ten gates green.

**Container: checked on macOS arm64 Docker Desktop only.** Both release images built
through the size gate. The native arm64 image prints `todo-mcp 0.1.0` for `--version`,
runs as User `65534:65534` with Volumes `/data`, Entrypoint `/todo-mcp`, Cmd `serve`
and a healthcheck, has no `TODO_MCP_CLIENT_ID` in Env, and fails `--entrypoint sh`. A
compose run with a fake client ID under `-p` (so the real
`microsoft-todo-mcp_todo-mcp-state` volume was untouched): `token` printed 64
characters, `doctor` showed `/data` owner `65534:65534` mode `0700`, `up -d` without a
login gave `Restarting (3)` with `AUTH_REQUIRED`, and `down -v` cleaned up.

**Linux: asserted by CI, not observed locally.** `docker stop` timing on the real
image, and the amd64 image on a native Linux engine, are asserted only by the
`Release image` job (see CI); an emulated amd64 image on an arm64 Mac is not evidence
for either. First-mount ownership of a fresh named volume was also observed on macOS
(above), and that is real evidence: Docker Desktop keeps named volumes on ext4 inside
its Linux VM, and only bind mounts go through VirtioFS's ownership remapping. CI
repeats it on amd64. Why it holds: a named volume mounted where the
image has no such path is created root:root 0755, but where it *does* exist, moby
copies the image directory's owner **and mode** onto the volume root before copying
entries. The Dockerfile's `COPY --chown=65534:65534 --chmod=0700 /data /data` is what
makes it exist, and `--chmod` is required — without it BuildKit's
`MkdirAll(target, 0755, chown)` discards the builder's `chmod 0700`. It needs a volume
(not a bind mount), its first mount, and an empty volume.

**Binary size** (`docs/footprint.md`). Measured 2026-09-15 on macOS arm64 Docker
Desktop, docker-driver builder, one platform per invocation: amd64 **3,931,280** bytes,
arm64 **3,468,312** bytes, both static non-PIE (`ET_EXEC`, `od -An -tx1 -j16 -N2` gives
`02 00`). The `FROM scratch` image is two layers and the same size as its binary.
`ARG SIZE_LIMIT=4915200` follows one rule: **the larger of the amd64/arm64 musl
binaries × 1.25, rounded up to a multiple of 64 KiB** (3,931,280 × 1.25 = 4,914,100 →
4,915,200). Both release builds passed it. The ARG default is the only value; when you
re-measure, update it and footprint.md in the same change. A cached `size` stage prints
no size line, so pass `--no-cache-filter size --progress=plain`. The ~336 KB figure in
early notes was a macOS build of the scaffold with rustls, ureq, tiny_http and
chrono-tz dead-stripped — not a baseline.

**Never run against a live Microsoft account.** That is the next step, and it is where
the design's UNVERIFIED items get settled. Run `login`, then `doctor`, then connect a
client, and record what happens in `docs/graph-probe.md` under the probe-output
redaction rule:

- `$batch` against `/me/todo/*` — `server.rs::sync_lists` falls back to sequential
  first pages on any 4xx from `$batch`, so a rejection degrades latency, not
  correctness. Confirm which path a real mailbox takes.
- `Prefer: outlook.timezone` on `/todo` — `graph::client::observe` records
  `timezone_mode` from `Preference-Applied`; `todo_account_status` reports it.
- Whether `null` clears `dueDateTime`/`startDateTime`/`recurrence` on PATCH
  (probes F/G). `todo_update_tasks` reports `cleared[]` on a 2xx; it does **not**
  yet re-read to confirm, so a silently ignored null would go unnoticed.
- Whether the hand-written `ALIAS_FALLBACK` Windows names in
  `domain/datetime.rs` are accepted on a date write (probe J). `UTC` is in that
  table too — it is the default zone and CLDR maps it to `Etc/UTC`.
- Which `protocolVersion` each client negotiates (record it in the README's Clients
  section).
- The exact granted `scope` string for a clean personal and a clean work account: any
  `.All`? Which extras (`User.Read`, `openid`/`profile`/`email`) appear? `vet_grant`
  refuses any granted `.All`, so a benign one would stop every first login.
- A real tool call from the musl release image: the first TLS handshake on a 512 KiB
  worker thread (`WORKER_STACK` in `http.rs`). Every smoke test so far stops before
  the network.
- Idle RSS (`docs/footprint.md`), `docker compose up` reaching `healthy`, and the
  revoke-consent click paths in `docs/app-registration.md`.

**Design deviations worth knowing** (each is deliberate; the specs do not describe
them):

- The cursor carries its filter arguments (`Cursor.f`), not only their hash: a
  cursor passed alone must still know what it was searching. Cursors are ~250
  characters instead of ~120.
- `todo_account_status` puts the "restart the server" instruction in a **second**
  `content` block, so `content[0].text` stays the exact JSON (m2 §7 and m2 §6
  conflict; both are satisfied this way).
- Integration tests reach Graph through `FixtureTransport`, a test-crate
  `GraphTransport` in `tests/common/mod.rs` that rewrites the Graph host to the
  fixture, not through `with_base` and the `test-fixtures` feature. `cargo test
  --locked` therefore needs no feature flag, and the origin check is exercised on the
  real `https://graph.microsoft.com` URLs.
- The inbound MCP bearer is a generated file (`<data dir>/bearer.token`), not an env
  var; `token` prints it and `serve` generates it if absent.
- `login` under compose: `docker compose run --rm todo-mcp login` is the documented
  form (it uses the compose-prefixed volume and `.env`), and it needs no TTY. Every
  runtime hint names both that and plain `todo-mcp login` through `LOGIN_HINT` /
  `RESTART_HINT`, because the README also documents a host toolchain.
- **Shutdown is installed before boot, and boot is not drained.**
  `cli::commands::Shutdown` is the first statement of `serve`, ahead of config, the
  `/data` probe, the bearer and the boot refresh (m7 §2 registers after them). Until
  boot completes (the token refresh and state construction, just before the listener
  binds), SIGINT/SIGTERM exit 0 at once: nothing is in flight, and the refresh can
  block for `TODO_MCP_HTTP_TIMEOUT_MS` plus a `.token.lock` wait. After that the first
  signal drains for up to 5 s and the second exits 0 immediately. A handler that
  cannot be installed is a startup refusal (exit 1, code `TRANSPORT`), never ignored.
  Only the refresh window of boot is regression-tested (`tests/shutdown.rs` parks the
  real binary on `.token.lock`); nothing proves `Shutdown` precedes config, the `/data`
  probe or the bearer.
- **Every requested stop exits 0**, matching README's Exit codes row 0: a signal during
  startup or a second signal exits without a `stopped` log line; requests still running
  when the 5 s drain ends are abandoned and logged as warn
  `drain deadline reached; abandoning in-flight requests` with an `unfinished` count,
  and `stopped` is still logged after it. That is safe because a refresh interrupted
  before its durable write leaves the old refresh token valid, and the kernel releases
  the flock.
- **`serve` can optionally start before sign-in.** With
  `TODO_MCP_START_WITHOUT_TOKEN=1`, follow mode is selected whether or not the boot
  refresh succeeds: missing tokens, Entra refusals and unreachable Entra keep health
  checks available; corrupt token files and refused grants still exit. The tool list
  is the configured scope ceiling, writes and account status pick up later logins
  without a restart, and refresh failures back off for 30 seconds per token file.
  No poller is needed because those calls compare the on-disk `token.json`.
- **The drain polls rather than nudging.** `run_http` calls `server.unblock()` once;
  the other workers leave on their 500 ms `recv_timeout`, so an idle stop takes about
  0.7 s at most (200 ms supervisor poll + 500 ms), tested at 2 s. Nothing respawns
  during a drain.

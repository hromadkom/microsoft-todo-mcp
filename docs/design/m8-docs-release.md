# M8 — Docs and release

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Handoff spec. M8 is **not** a rewriting milestone: [`README.md`](../../README.md) already
leads with the least-privilege argument and already carries the client matrix, the tool
table, the configuration table and a Security section. M8 fills the gaps *around* it, creates
the four documents that shipped files already point at, and reconciles what M1–M7 proved
wrong. Two findings below are quickstart bugs that make the exit gate unpassable today.

**Exit gate.** `README.md` leads with the least-privilege argument · `SECURITY.md`
carries the CAN/CANNOT table, every row mapped to the gate or code path that
enforces it · **a second person** reaches a first tool call in **under 15 minutes**
from `README.md` and [`docs/app-registration.md`](../app-registration.md) alone.

**Files:** `SECURITY.md`, `CHANGELOG.md`, `CONTRIBUTING.md`,
`docs/{operations,footprint,clients,graph-probe}.md`, `.github/workflows/release.yml`,
`.github/{dependabot.yml,pull_request_template.md}`; edits to `README.md`,
`AGENTS.md`, `compose.yaml`, `docs/design/README.md`.

---

## 1. What M8 inherits, and the four references that already dangle

Present and **not** to be recreated: [`README.md`](../../README.md), [`AGENTS.md`](../../AGENTS.md),
`LICENSE`, [`docs/app-registration.md`](../app-registration.md), `.env.example`,
[`scripts/gates.sh`](../../scripts/gates.sh), [`ci.yml`](../../.github/workflows/ci.yml)
+ its `hermetic-gate` composite action, [`Dockerfile`](../../Dockerfile),
[`compose.yaml`](../../compose.yaml), `clippy.toml`, `rust-toolchain.toml`.

**The gate count is stale in two shipped files.** `README.md` and `AGENTS.md` both say *nine*
structural invariants; [`m5-agenda-timezone.md`](m5-agenda-timezone.md) §3 adds a tenth — a grep
gate proving `domain/datetime.rs` is the only caller of `from_local_datetime` — which lands three
milestones before this one. Reconcile the number wherever it appears.

Four documents M8 is responsible for are **already referenced by shipped files** — broken links
today:

| Document | Referenced from | What the reference promises |
|---|---|---|
| `docs/footprint.md` | `Dockerfile:67`, `.github/workflows/ci.yml:36` | the real musl number `SIZE_LIMIT` ratchets toward |
| `docs/graph-probe.md` | `AGENTS.md:63`, `scripts/gates.sh:34`, [`m2-mcp-http.md`](m2-mcp-http.md) lines 13 and 274 | the live `$filter`/`$orderby` verdict that justifies gate 2. **M2's exit gate writes this file** — M8 owns its shape (§6), not its creation |
| `SECURITY.md` | `AGENTS.md:96`, `scripts/gates.sh:4`, `scripts/gates.sh:30` | *"a failing gate means the docs became untrue"* — gate 1 names it |
| `docs/operations.md` | nothing in `src/` today | `src/cli.rs:58` points `--help` at the README's Configuration section instead; repoint it once the file exists |

> **Do not create `THREAT_MODEL.md`.** `scripts/gates.sh` and `AGENTS.md` already commit
> to `SECURITY.md` at the repo root for the CAN/CANNOT document. A long-form
> `docs/threat-model.md` behind it is optional and **not** v1-blocking.

---

## 2. The quickstart does not work yet — two ordering bugs

Both are exit-gate blockers, hit in the first two minutes of following the README.

> **The `login` volume and the `serve` volume are different volumes.** README step 1 is
> `docker run --rm -it -v todo-mcp-state:/data … login`, using a volume literally named
> `todo-mcp-state`. `compose.yaml` sets `name: microsoft-todo-mcp` and declares
> `todo-mcp-state:` with no explicit `name:`, so Compose creates and mounts
> **`microsoft-todo-mcp_todo-mcp-state`** — its own reset comment (`compose.yaml:45`)
> already spells the prefixed name out. The login succeeds, writes `token.json` into the
> *unprefixed* volume, and `docker compose up -d` then starts a server that refuses to boot
> with `NotLoggedIn` against an empty `/data`. Nothing in the error names the cause, and
> `Dockerfile:60` compounds it by printing the unprefixed `docker volume rm todo-mcp-state`
> as "the documented reset" — which removes nothing a compose-started server ever touched.
>
> Pick one remedy and make every document agree: give the compose volume an explicit
> `name: todo-mcp-state`, **or** document the login as
> `docker compose run --rm todo-mcp login`, which mounts the right volume, inherits
> `env_file`, and gets a TTY because `compose run` allocates one by default. The second is
> **UNVERIFIED** — Docker has never been built in this repo; confirm `read_only: true` and
> `user: "65534:65534"` do not interfere first. State the asymmetry alongside: `token`
> needs `-T` *because* its output is captured; `login` needs the TTY *because* it is
> interactive. Reconcile the printed order too: [`m2-mcp-http.md`](m2-mcp-http.md) §6
> **prescribes** `token` → `login` → `up -d`, because `tools/list` is frozen at boot;
> `compose.yaml`'s header already follows it and the README still prints
> `login` → `up -d` → `token`. The README is the one out of step — and it is the document
> the exit gate is timed against.

> **Quickstart step 1 uses an image nothing has built.** `microsoft-todo-mcp:latest` has
> no registry prefix, so it resolves locally only — on a fresh clone the first command
> fails before the `compose up` that would have built it. M8 must choose the install path,
> and the choice gates the stopwatch: **pull** a published image
> (`docker pull <registry>/<image>:<version>`, making §9's registry prerequisites blocking)
> or **build** locally (`docker compose build todo-mcp`). A cold LTO release build is not
> a quarter-hour budget item.

---

## 3. `SECURITY.md` — the CAN/CANNOT table and what enforces each row

The table is the centrepiece, and **every row names the thing that makes it true** — that
mapping is why `scripts/gates.sh` opens with *"a failing gate means the docs became untrue"*.

| This server CAN | Enforced / bounded by |
|---|---|
| Read and write **your** To Do lists and tasks | one delegated scope; write tools registered only when the scope **read back from the token response** says `Tasks.ReadWrite` ([`m1-auth.md`](m1-auth.md) §4, [`m6-write-tools.md`](m6-write-tools.md) §7) |
| Hold one delegated refresh token at `/data/token.json`, mode 0600 in a 0700 dir | `create_new(true).mode(0o600)` sets the mode **at creation**; `config::validate()` refuses to start on a loose or unwritable `/data` ([`m1-auth.md`](m1-auth.md) §5) |
| Reach exactly two hosts: `login.microsoftonline.com` and `graph.microsoft.com` | gate 3 confines the `Authorization` header to `graph/client.rs` and `http.rs`; gate 4 proves `with_base()` is never called from shipping code; `@odata.nextLink` is origin-checked before the bearer is attached |
| Serve MCP on the configured bind, bearer-authenticated | the guard ladder in [`m2-mcp-http.md`](m2-mcp-http.md) §3 — bearer, then `Origin`, then `Host`, then body cap |

| This server CANNOT | Enforced by |
|---|---|
| Read anyone else's tasks | gate 1 — `/users/{id}/…` occurs **zero** times in `src/`. Only `/me`. |
| Read your mail, files, calendar, contacts or profile | `config::validate()` accepts only `Tasks.Read` or `Tasks.ReadWrite`; any `.All`, and a set `TODO_MCP_CLIENT_SECRET`, are startup refusals |
| Act without you | app-only is not configured — and app-only **cannot write To Do at all**: Microsoft documents `"Not supported."` for every To Do write operation, so a delegated token is the only kind that exists for writes |
| Learn your name or email | `openid`/`profile` are not requested and `/me` is never called for the user object; `account_id` is the literal string `"default"` ([`m1-auth.md`](m1-auth.md) §4) |
| Persist task content | the cache is memory-only and dies with the process ([`m4-cache-read-tools.md`](m4-cache-read-tools.md) §1). The `/data` inventory is `token.json` plus `.token.lock` — **and whatever the `token` subcommand persists, which §7 says is still unsettled.** Settle that before this row is written; do not ship "the only credential file is `token.json`" on an assumption |

The refresh-token section carries one verbatim Microsoft statement that must not be
paraphrased away — the sole justification for releasing the flock across the network
redeem ([`m1-auth.md`](m1-auth.md) §6):

> *"The Microsoft identity platform doesn't revoke old refresh tokens when used to fetch
> new access tokens. Securely delete the old refresh token after acquiring a new one."*

So: **no reuse-detection cascade**. The real risks are local last-writer-wins clobbering,
closed by the lock-then-re-read store, and unbounded RT fan-out, bounded by one long-lived
process plus adoption.

The honest-authorization section does not soften: MCP authorization is *optional* and the
specification is **silent** on static bearer tokens — out of scope, not blessed. Claiming
conformance invokes a SHOULD for OAuth 2.1 and a MUST for RFC 9728 protected-resource
metadata, neither of which v1 ships. Bind to loopback, and do not let `http.rs`'s module
doc repeat [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s "spec-legal minimum" phrasing. §10 holds the rest, the **no Graph
permission GUID anywhere** rule ([`../../AGENTS.md`](../../AGENTS.md)) included: three
sources gave three different values for `Tasks.ReadWrite`, sharing a prefix and diverging
in the last segment — the signature of a fabricated tail. Users tick a checkbox by name.

> **The `sanitize_text` row from the earlier unpublished research has nothing behind it.** The raw table
> lists *"Log task titles, list names, tokens, or authority responses"* as a CANNOT
> enforced by `sanitize_text`. That helper does ship here — `src/errors.rs:170`,
> `sanitize_text(text, secrets: &[&str])` — but it replaces **caller-supplied secret
> substrings** of 8+ chars with `<redacted>`: it can scrub a bearer out of a third-party
> error string and **cannot** scrub a task title, which is not a known secret. Nor is there
> a test — `tests/` holds `cli_smoke.rs` alone, and `secret_leak.rs`
> ([`m1-auth.md`](m1-auth.md) §7) pins token material. Split the row (tokens and authority
> responses stay; task titles and list names come out) or write the gate. A CANNOT with
> nothing behind it is what `gates.sh` exists to prevent.

---

## 4. `docs/clients.md` — a table of things nobody has published

Its reason to exist is the recorded `protocolVersion` per client. **No authoritative
published list exists**, so this is the only evidence for whether the 2026-07-28 protocol
era ever earns its keep ([`m2-mcp-http.md`](m2-mcp-http.md) §1 records that Claude Code
probes it, gets a clean method-not-found and falls back — the result that makes
legacy-only safe today). Cells blank and labelled, never guessed.

| Client | How it is pointed here | Negotiated `protocolVersion` | Date recorded | Status |
|---|---|---|---|---|
| Claude Code | `claude mcp add --transport http … --header` | *(blank — M2 records)* | — | ✅ verified end-to-end against a legacy-only server |
| Claude Desktop → Code tab | same; it *is* Claude Code | *(blank — M2 records)* | — | ✅ asserted from the locked decisions, not independently verified |
| Claude Desktop → chat | — | n/a | — | ❌ config schema requires `command`; `url`/`headers` entries are silently stripped, and remote connectors run from Anthropic's cloud and cannot reach localhost |
| Hermes Agent | static `Authorization` header on an http transport | *(blank — M2 records)* | — | ⏳ **Untested** |
| `mcp-remote` bridge | `--allow-http --transport http-only` | *(blank)* | — | ⏳ flags unverified |

> **Hermes stays `⏳ Untested` until an M2 result says otherwise.** The earlier unpublished research marks
> it `✅`; that is wrong, and it is a kill-risk gate — Hermes is the reason HTTP was in
> scope at all, so a failure there is a finding, not a footnote. **No `mcp-remote` version
> number** either: the `0.2.1` in that research has no citation, and the two flags the README
> carries must be **verified against mcp-remote's own documentation** before being
> restated as required.

---

## 5. `docs/footprint.md` — lead with the dead-strip trap

> **The ~336 KB M0 binary is not a baseline and must not be quoted as one.** LTO
> dead-strips rustls, ureq, tiny_http and chrono-tz because the M0 skeleton never calls
> them, so anyone ratcheting `SIZE_LIMIT` toward it sets a gate the first real build cannot
> pass. Put this in the file's first paragraph, above every table.

Measured during design — both **macOS aarch64 Mach-O, not static musl ELF**:

| Measurement | Bytes | Dependency set |
|---|---|---|
| Partial | 1,451,136 | serde_json + ureq 3.4 (rustls) + tiny_http + webpki-roots |
| Full v1 set + ~120 LOC | **2,573,088** | the above plus chrono (`std`,`now`), chrono-tz, windows-timezones, signal-hook, subtle |

Both at `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "unwind"`,
`strip = true`, on rustc 1.97.1. An earlier 1,302,496 B figure circulated in that unpublished research;
it omitted chrono-tz's compiled tzdb and webpki-roots — **do not quote it**. Static
musl will be larger, and ~4,500 LOC with serde derives and ten inline JSON schemas
typically adds 0.5–1.5 MB. Record per architecture: bytes, command, date, rustc version.
A plain `docker build --target size` on an arm64 host measures **one** arch; both arches
need buildx *and* a container builder ([`m7-container.md`](m7-container.md) §4):

```
docker buildx create --driver docker-container --use
docker buildx build --platform linux/amd64,linux/arm64 --target size --output type=cacheonly .
```

`SIZE_LIMIT` ships at 8388608 **twice** — `ARG` in the `Dockerfile`, `build-args:` in
`ci.yml` — and [`m7-container.md`](m7-container.md) §6 rules the `build-args` line deleted
so the `ARG` is the single source. Record the survivor, set from the first musl build at
measured × 1.25, then ratchet. If it proves tight the cut ladder is `webpki-roots`
(−148 KB) → `panic = "abort"` (−133 KB, killing the panic contract in
[`m2-mcp-http.md`](m2-mcp-http.md) §4) → jiff over chrono + chrono-tz (−841 KB, re-opening
[`m5-agenda-timezone.md`](m5-agenda-timezone.md) §3) — but re-price the first rung, whose
stated cost was the already-cut `TODO_MCP_EXTRA_CA_FILE`.

**Idle RSS is UNVERIFIED.** `compose.yaml` sets `mem_limit: 96m` and `memswap_limit: 96m`
— equal on purpose, which disables swap so the in-memory access token cannot be paged to
disk. Measure with `docker stats --no-stream` cold and after one `todo_lists`; record both
with the date. **There is no target number yet.** [`m7-container.md`](m7-container.md) §12
sets none — it records [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s ~4 MB image / ~3 MB idle RSS and says *"this project has
no comparable figure"*. Do not restate that figure as a target or as achieved; the first `docker stats` reading
is what this file gets to assert.

---

## 6. `docs/graph-probe.md` — redacted, never a raw transcript

Holds the output of the M2 probe suite: **A–D and F–L**
([`m2-mcp-http.md`](m2-mcp-http.md) §10 defines eleven, not twelve — there is no probe E in it;
`$expand=checklistItems` is numbered E in [`m4-cache-read-tools.md`](m4-cache-read-tools.md) §10
and is optional, so give it a section only if it is actually run). One `##` section per probe,
this shape:

```
## Probe C — $filter=zzNotAProperty eq 'x'
Date · account type · list pseudonym · list shape (n tasks, o open, d done)
Request:      exact method, URL and headers sent (Authorization elided, ids pseudonymised)
Response:     status line, response headers, body — verbatim apart from the redactions below
Verdict:      honoured | silently ignored | hard error
Consequence:  the one line of code or docs this verdict changes
```

> **Probe C outranks probe B.** A 200 on a filter over a property that does not exist
> proves the service is not validating filters at all, which forces the "silently ignored"
> verdict for B *even when B's result set looked correct*. Write the two verdicts in that
> dependency order so a later reader cannot take B alone.

Gate 2 (`no $filter/$orderby under src/graph/`) relaxes to a warn-level lint on exactly
one outcome — B returns exactly `ids_open` **and** C is 4xx — and stays hard-fail on
every other combination, C returning 200 included. Client-side filtering ships in
**every** case ([`m4-cache-read-tools.md`](m4-cache-read-tools.md) §5); the probe changes
only the gate's severity. Until this file exists no document may state either `$filter`
behaviour as fact — `AGENTS.md:63` and `scripts/gates.sh:34` already word it that way.

**Redact all user content; never commit a raw transcript.** The probes walk a **real**
mailbox, so user content reaches the output. Replace **all** of it — task titles, bodies,
list names, checklist items, categories, account names and email addresses — with typed
placeholders such as `«title»`, and replace every id (list, task, checklist item, tenant,
client) with a consistent pseudonym (`L1`, `T1`, …) so references still line up. Keep
status lines, header names, timestamps, error codes and the response shape: the shape is
the entire point. Say at the top of the file that it is redacted, and redact before
anything is written into the repository — a raw transcript is never committed, not even
temporarily.

---

## 7. `docs/operations.md` — what a stuck operator needs

Outline, in the order someone in trouble reads it ([calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp) has no such document to copy):

| Section | Content | Waits on |
|---|---|---|
| Reading `doctor` | annotated real output: resolved config, `/data` owner + octal mode, token file mode, granted scopes, absolute `expires_at`, Graph reachability. **`doctor` never refuses to run** — it is what you reach for when the config is already broken, so a missing `TODO_MCP_TZ` is a finding with a remediation, not an exit | M1 |
| `/data` permissions | the remediation from [`m1-auth.md`](m1-auth.md) §5 — `docker run --rm -u 0 -v todo-mcp-state:/data busybox chown -R 65534:65534 /data` — with **two corrections it needs before it is printed anywhere**: append `&& chmod 0700 /data`, because `chown -R` leaves the root `0755` and fails the `0700` clause ([`m7-container.md`](m7-container.md) §3), and name the **prefixed** volume `microsoft-todo-mcp_todo-mcp-state` under compose, or it chowns a volume the server never mounts — §2's bug again. An external image is required **because this one has no shell**. Plus the no-chown alternative: bind-mount a host directory the invoking user owns | [`m7-container.md`](m7-container.md) §8 |
| Volume reset | `docker volume rm microsoft-todo-mcp_todo-mcp-state` — the **prefixed** name; `Dockerfile:60` prints the unprefixed one and removes nothing, so fix both with §2. Ownership transfers from the image only on the **first** mount of an **empty** volume ([`m1-auth.md`](m1-auth.md) §5). `docker compose down -v` deletes the token — say that in bold | [`m7-container.md`](m7-container.md) §8 |
| Restart semantics | `restart: unless-stopped`, `stop_grace_period: 10s`; the healthcheck is `/todo-mcp healthcheck`, a raw `TcpStream` `GET /healthz`, mandatory because the scratch image has no curl. Logs are JSON on stderr only — `json-file`, `max-size 10m`, `max-file 3` — with a field reference | M1 |
| `restart_required` | on a `None`/`ReadOnly` → `ReadWrite` transition detected during a refresh, the server warns and `todo_account_status`'s **text** says `"Restart the server (docker compose restart todo-mcp) to expose the write tools."` The model cannot restart a container — this section explains why the tool says that | M2 §6 |
| Backup, restore, rotation | copy `token.json` preserving mode; a restored token whose `client_id` or `requested_scope` disagrees with the running config is refused, not silently used. Separately: how to invalidate and reissue the **inbound** MCP bearer, which is never forwarded to Graph — blocked, see below | M1 §7 tests 6–7 |
| Outbound network | ureq silently honours `HTTPS_PROXY`/`ALL_PROXY` via `Proxy::try_from_env()`. For a process holding OAuth bearers that must be a documented decision, not an accident ([`m3-graph-client.md`](m3-graph-client.md) §1). Carries the v2 note for `TODO_MCP_EXTRA_CA_FILE` and the ureq `TlsConfig` path, cut from v1 | — |
| Compose `secrets:` | the long form **parses under Compose v5.4.0** (verified); whether the spec's `uid`/`gid`/`mode` are honoured at *runtime* is **UNVERIFIED**, so the `chmod 0444` caveat ships marked as such | — |

> **Where the inbound MCP bearer lives is not settled.** `src/cli.rs:48` ships a `token`
> subcommand — *"Print the inbound MCP bearer token (no trailing newline)"* — and
> `compose.yaml` documents capturing it, but neither [`m1-auth.md`](m1-auth.md) §5 nor
> [`m2-mcp-http.md`](m2-mcp-http.md) §3 says whether it is persisted under `/data`. If it is
> a second file there, `SECURITY.md`'s `/data` inventory must list it, call it a **local**
> credential and not a Microsoft one, and say whether `logout` sweeps it — and the rotation
> row collapses to "delete that file and restart". Settle it first.
>
> `account.json` **does not exist** — the adversarial review deleted it: a persisted
> `mailbox_id` helps neither a memory-only cache nor a 900 s cursor, `logout` never swept
> it, and it went stale after logout→login-as-another-account. Never list it under `/data`.

---

## 8. `CHANGELOG.md` and `.github/workflows/release.yml`

Keep a Changelog 1.1.0 + SemVer, [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s shape: `[Unreleased]` on top,
`### Added / Changed / Fixed`, compare-link footers (unwritable until the remote exists).
`[Unreleased]` accumulates M1–M7 from the user-visible side — "device-code sign-in", not
"added `auth/device_code.rs`". **First release is `0.1.0`**: `Cargo.toml` line 3 already
declares it and the workflow refuses to publish when tag and manifest disagree, so the
first tag is `v0.1.0` and the first heading `## [0.1.0]`. [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp) opened at 0.3.0
after two private releases; do not copy it.

The workflow is [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s with three substitutions — project name, the OCI
`description` label, and the default `IMAGE_NAME` (`hromadkom/microsoft-todo-mcp`, matching
`Cargo.toml` line 12's declared `repository`):

- `release: types: [published]`, plus `workflow_dispatch` taking an existing tag, to
  re-run after fixing registry credentials.
- **verify** — check out the tag, strip the `v`, assert
  `^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$`, compare against
  `grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2`, **fail on mismatch**; *warn* on a
  missing `## [<version>]` in `CHANGELOG.md`; then run the hermetic gate.
- **publish** — buildx, **no QEMU** (the builder stage is `$BUILDPLATFORM`-pinned and
  cargo-zigbuild cross-compiles both musl targets natively); tags
  `{<version>, <short-sha>, latest}` with `latest` off for pre-releases;
  `platforms: linux/amd64,linux/arm64`; `provenance: mode=max`; **no SBOM** — a
  `FROM scratch` image has no package list for a scanner to attest.

> **`write-cache: "false"` on the release path is not a tuning choice.** A BuildKit cache
> exported from a **tag ref** is not readable from `main` or from a pull request, so
> exporting there writes an entry nothing can ever restore while still consuming the shared
> 10 GB budget; `ci.yml:23` already names `main` the sole producer. Relatedly,
> `uses: ./.github/actions/hermetic-gate` resolves from the checked-out tree, which here is
> the **tag** — a tag cut before that action existed cannot be re-run via `workflow_dispatch`.

> **Renaming a job breaks the required status checks.** The earlier unpublished research
> lists them as `Hermetic gate` and `Release build (musl static) + size gate`; the
> **shipped** `ci.yml` names them `Hermetic gate` and `Release image`. Configure the ruleset
> from the shipped names, or `main` is unmergeable against checks that never report. Create
> the ruleset only **after** the first release: until then an amended, force-pushed `main`
> stays available as the recovery path for a failed CI or release run.

---

## 9. Repository prerequisites, and the hygiene files

The repo has **one commit** (`a867485`), a clean tree and **no git remote**. §8 is
inert until these are done, and 1–4 gate the stopwatch if §2's pull path is chosen.

1. Create the GitHub repository as `hromadkom/microsoft-todo-mcp` — `Cargo.toml` line 12
   already declares that `repository`/`homepage`, so any other name makes the manifest
   wrong on day one. Then `git remote add origin …` and push `main`.
2. The `Protected main` ruleset, required checks named **exactly** as `ci.yml` names its
   jobs (`Hermetic gate`, `Release image`; §8), is created only **after** step 5's first
   release, so the amend-and-force-push recovery path stays open until the release is proven.
3. Repository *variables* `REGISTRY` (`docker.io`) and `IMAGE_NAME`
   (`hromadkom/microsoft-todo-mcp`) so forks publish to their own namespace; *secrets*
   `DOCKERHUB_USERNAME` / `DOCKERHUB_TOKEN` — an access token, not the password.
4. Enable **private vulnerability reporting**; `SECURITY.md` links
   `…/security/advisories/new`, which 404s until it is on. Then Dependabot alerts.
5. Only then publish a GitHub Release tagged `v0.1.0`. `Cargo.toml` sets `publish = false`,
   so there is no crates.io step.

`CONTRIBUTING.md` is [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s with this project's conventions substituted:
`docker build --target test .` as the one gate; the dockerized dev loop; the
`TZ=Pacific/Kiritimati` re-run for anything touching dates; Conventional Commits;
`CHANGELOG.md` under `## [Unreleased]`; **do not bump the version yourself**. Restate
[`AGENTS.md`](../../AGENTS.md)'s four conventions — stdout belongs to `cli/out.rs`, two
credentials never conflated, no panics across the tool boundary, no `chrono::Local`.

`dependabot.yml` is [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s minus the `rrule` ignore block, keeping the explicit
`/.github/actions/hermetic-gate` directory entry (`directory: "/"` would leave the pins
inside `.github/actions/*/action.yml` silently un-updated) and the warning that Dependabot
cannot see `compose.yaml`'s `dockerfile_inline` pin of `rust:1.97.1-slim`, hand-synced with
`rust-toolchain.toml`. `.github/ISSUE_TEMPLATE/*` is **optional and not v1-blocking**; if
added, every link out of a form must be an **absolute URL** — a form renders at
`/issues/new`, where a repo-relative path does not resolve, which would put "never paste
your credential" out of reach of the exact form where credentials get pasted. [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s
`bug_report.yml` links `SECURITY.md` by full `https://github.com/…/blob/main/…` for that.

---

## 10. Claims no document may make

Every one was refuted or is unverified, and the README states them all correctly today —
M8's job is to **not regress them** while editing around them.

| Do not write | Write instead |
|---|---|
| the token file is encrypted | plaintext, mode 0600, protect the volume |
| the static bearer is spec-legal / MCP-conformant | out of scope, the spec is silent, a deliberate documented deviation |
| `Secret` guarantees erasure | best-effort without `zeroize` |
| "four concurrent requests" as a To Do fact | conservative inference — the published limit is against the **legacy** `outlookTask`/`outlookTaskFolder` types, not `todoTask` |
| `$filter`/`$orderby` works (or silently fails) on `todoTask` | **UNVERIFIED** until `docs/graph-probe.md` exists ([`m3-graph-client.md`](m3-graph-client.md) §11) |
| Claude Desktop chat connects | its config schema requires `command`; the **Code tab** connects because it is Claude Code |
| Hermes ✅ | `⏳ Untested` until M2 |
| `TODO_MCP_TZ` is required with no default | defaults to UTC **with a loud warning** — the README and `.env.example` already shipped that ([`m5-agenda-timezone.md`](m5-agenda-timezone.md) §2) |
| `THREAT_MODEL.md`; `account.json` under `/data`; a Graph permission GUID; an `mcp-remote` version | `SECURITY.md` at the repo root; `account.json` does not exist; the permission name; the two flags, once verified |

---

## 11. Running the 15-minute test

*"A second person"* and *"from the README and `docs/app-registration.md` alone"* are both
load-bearing: the author cannot run this, because the failures being measured are knowledge
the author already has. A cold machine with Docker and nothing else; splits at app
registration complete · `login` complete · `/healthz` answering · first tool call returning
real data. Record stalls verbatim and fix the document, not the person. Run it on a
**personal MSA** — the supported v1 path; work/school is documented-but-not-guaranteed,
because Microsoft's managed consent policy (**the default for a new tenant**) excludes
`Tasks.Read` and `Tasks.ReadWrite` from end-user consent, so a work account stalls on an
admin grant. Beyond §2's two bugs and a missing `-T`, the likeliest stall is the
`allowPublicClient` / "Allow public client flows" toggle — the #1 first-run failure
(AADSTS7000218).

---

## 12. Settle during M8

- **Pull or build?** §2 — it decides whether §9's registry steps block the exit gate.
  Decide before anyone starts a stopwatch.
- **The `login` volume mismatch** — one remedy, applied to `README.md`, `compose.yaml`,
  `Dockerfile:60` and `docs/operations.md` in one change.
- **Where the inbound MCP bearer is persisted** — unsettled by M1 and M2; both
  `SECURITY.md`'s `/data` inventory and the rotation section are wrong if guessed.
- **The "never logs task titles" CANNOT row** — `sanitize_text` scrubs *known secrets*,
  not titles, and no test covers it (§3). Split the row or write the gate.
- **`mcp-remote`'s `--allow-http --transport http-only`** — verify against mcp-remote's
  own docs before restating them as required; no version number.
- **Idle RSS** and **Compose `secrets:` at runtime** — **UNVERIFIED**. Nothing Docker
  here has ever been built, so the RSS number does not exist; the `secrets:` long form
  parses under Compose v5.4.0 but `uid`/`gid`/`mode` honouring is untested.
- **Can a bare personal Microsoft account register an app at entra.microsoft.com, or
  must it create a free Azure account first?** *"Try first, fall back"* until tried cold.
- **Which user-consent policy a newly created tenant defaults to**
  (`microsoft-user-default-low` vs `-legacy`). The *exclusion* of `Tasks.Read`/`Tasks.ReadWrite`
  from end-user consent is verified against Learn ([`../../AGENTS.md`](../../AGENTS.md)) and
  is what §11 rests on; only the identifier is open — and it, not the permission's own
  "Admin consent required: No" flag, decides self-consent. No document asserts one.
- **The footprint numbers** come from [`m7-container.md`](m7-container.md); M8 only records
  them. Do not release before that milestone has filled `docs/footprint.md` in.

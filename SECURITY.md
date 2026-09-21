# Security Policy

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately through GitHub's
[private vulnerability reporting](https://github.com/hromadkom/microsoft-todo-mcp/security/advisories/new)
(Security → Report a vulnerability). If that is unavailable to you, email
`hromadko.m@gmail.com` with `microsoft-todo-mcp` in the subject.

Please include:

- the version (`todo-mcp --version`) or image tag and digest;
- how you run it: the shipped `compose.yaml`, plain `docker run`, or a host binary;
- the account type (personal Microsoft account, or work or school) and `TODO_MCP_SCOPE`;
- the smallest set of steps that reproduces the problem, and what you expected instead.

**Redact before you send anything**, even privately, because a report may later be
shared with Microsoft or published as an advisory:

- never include `token.json`, `bearer.token`, the output of `todo-mcp token`, an
  `Authorization` header, a refresh or access token, or a device code;
- replace your client ID, tenant ID, account name and email with placeholders;
- replace list names, task titles and any other task content, which `doctor` and
  the logs of a failing request can contain, with placeholders.

This is a personal side project, not a commercial product: expect an
acknowledgement within about a week, and a fix timeline that depends on
severity. There is no bug bounty. A vulnerability in Microsoft Entra ID or
Microsoft Graph themselves belongs with Microsoft, not here.

## Supported versions

Only the latest release receives fixes. Older tags are not patched.

## What the data directory holds

The container keeps its state in the `/data` volume (`TODO_MCP_DATA_DIR` outside a
container). The image creates `/data` owned by uid 65534 with mode `0700`; on a host
run, the server creates the directory `0700` if it is missing. A directory looser than
`0700` is reported as a warning, not refused. `serve`, `login` and `token` refuse a
directory they cannot write.

| File | What it is | Protection |
|---|---|---|
| `token.json` | Your Microsoft sign-in: one delegated **refresh token**, stored **in plaintext**, plus the client ID, authority and scopes it belongs to. The access token is never written to disk. | Created mode `0600` through a temporary file in the same directory and an atomic rename. It is **not encrypted**. Protect the volume and treat the file like a password. |
| `bearer.token` | The inbound MCP bearer your clients send to `/mcp`: 32 random bytes, hex-encoded. It is a **local** credential, unrelated to your Microsoft account, and is never sent to Microsoft. | Created mode `0600` when the server generates it (on the first `token` or `serve`). If `TODO_MCP_BEARER_FILE` points at a file you supply, the server reads it as-is and does **not** check its mode or owner. |
| `.token.lock` | An empty lock file. Every read and write of `token.json` takes an exclusive lock on it, so a `login` and a running server's refresh cannot overwrite each other. | Created mode `0600`. Holds no secret. Nothing removes it, neither `logout` nor a dead-token deletion: unlinking a lock file another process holds would let two processes in at once. |
| `token.json.tmp.*` | Short-lived files from the atomic write. | Created mode `0600`. |

Consequences:

- **`logout` deletes the Microsoft sign-in only**: `token.json` and any stale
  `token.json.tmp.*`, under the `.token.lock` lock, so a refresh already running at
  that moment cannot write the sign-in back. It **keeps `bearer.token`**, and says so
  when that file exists.
- **`logout` does not revoke the refresh token at Microsoft.** For a personal account,
  remove the app at <https://account.microsoft.com/privacy/app-access>. For a work or
  school account, ask your administrator. See
  [Revoking consent](docs/app-registration.md#revoking-consent); neither path has been
  verified against a live portal, and this project has not verified which
  administrator action invalidates a refresh token that was already issued.
- **To rotate the MCP bearer**, delete `bearer.token` and restart `serve`. `serve` reads
  the bearer once at startup and generates a new one if the file is missing. Then run
  `token` again and update the `Authorization` header in every client. The old bearer
  keeps working until that restart. The README's
  [Signing out and rotating the MCP bearer](README.md#signing-out-and-rotating-the-mcp-bearer)
  section has the commands.
- **`docker compose down -v` deletes the volume**, and with it both your sign-in and
  your MCP bearer.
- When Microsoft reports the refresh token dead during a refresh (AADSTS70008, 700082
  or 530036), `serve` and `doctor` delete `token.json`, but only if no newer sign-in has
  replaced it in the meantime.
- In follow mode (`TODO_MCP_START_WITHOUT_TOKEN=1`), a later `login` is noticed by the
  next write or account-status call without a restart. A transient refresh failure is
  retried at most every 30 seconds for the same `token.json`; replacing that file retries
  immediately.

### Why a leaked refresh token outlives rotation

One Microsoft statement shapes the token store, and it is quoted here verbatim:

> *"The Microsoft identity platform doesn't revoke old refresh tokens when used to fetch
> new access tokens. Securely delete the old refresh token after acquiring a new one."*

The server replaces `token.json` whenever Microsoft returns a new refresh token. The
old one is not revoked. **A copy of `token.json` taken earlier keeps working** until
it expires or you revoke the app's consent. Rotation on the server's side does not
contain a leak.

## What this server can and cannot do

Every row names what makes it true. A row backed by a gate in
[`scripts/gates.sh`](scripts/gates.sh) fails the build when it stops being true. A
regression in any row is a security bug worth reporting.

| This server CAN | Enforced or bounded by |
|---|---|
| Read the To Do lists and tasks of the one account that signed in | The only scopes it ever requests are `Tasks.Read` or `Tasks.ReadWrite`, plus `offline_access`. `TODO_MCP_SCOPE` accepts nothing else; any other value is a startup refusal (`src/config.rs`). |
| Create, update, complete and delete those tasks, and edit their checklists | The five write tools are registered only when the grant allows writes **and** `TODO_MCP_SCOPE` is `Tasks.ReadWrite`: `auth::vet_grant` caps the grant at `TODO_MCP_SCOPE` on every sign-in and refresh, and `serve` fixes the tool list from it at startup (or from the configured ceiling under the opt-in `TODO_MCP_START_WITHOUT_TOKEN=1` mode). The grant comes from the scope Microsoft reports in the token response; if Microsoft omits it, the requested scope is assumed. Under `Tasks.Read` the write tools are absent from `tools/list`, and a call to one is refused. `todo_delete_tasks` also requires `confirm: true` and refuses the `flaggedEmails` list. |
| Hold one delegated refresh token, in plaintext, in `/data/token.json` | See [What the data directory holds](#what-the-data-directory-holds). |
| Send your Microsoft tokens to `login.microsoftonline.com` and `graph.microsoft.com`, over HTTPS | Both hosts are fixed in the code, and both HTTP agents are built HTTPS-only with redirects disabled. An `@odata.nextLink` is a server-controlled URL, so before the access token is attached to it, `graph/paging.rs` requires scheme `https` and authority exactly `graph.microsoft.com`, with no port and no userinfo. The same check runs on the `@odata.nextLink` of every `$batch` sub-response. **Gate 3** confines the `Authorization` header to `graph/client.rs` and `http.rs`, so no second call site can skip that check. **Gate 4** proves `with_base()`, which points a client at another host, is never called from shipping code. **Gate 9** keeps the `test-fixtures` feature, which unlocks it and cleartext transport, out of the default build. A proxy changes the network path; see [Outbound proxy](#outbound-proxy). |
| Serve MCP on its configured address, to clients holding the bearer | The guards in `http.rs`, outermost first, so an unauthenticated request is rejected before any work: the bearer (compared in constant time) → 401. Then `Origin`: absent is allowed; present, it must be a localhost variant or match `Host`, else 403. Then `Host`, which must be `localhost`, `127.0.0.1`, `[::1]`, the bind address when it is a specific one (not `0.0.0.0` or `::`), or a `TODO_MCP_ALLOWED_HOSTS` entry, else 403. Then a body over 1 MiB → 413. `GET /healthz` is unauthenticated and returns only `{"status":"ok","version":…}`. |

| This server CANNOT | Enforced by |
|---|---|
| Read anyone else's tasks | **Gate 1**: the `/users/{id}/…` path shape occurs nowhere in `src/`. Apart from the `$batch` endpoint itself, every Graph path the code names is under `/me/todo/`, and so is every `$batch` sub-request. |
| Keep a token that carries an organisation-wide `.All` permission | `auth::vet_grant` runs before anything is written. If the scope Microsoft reports includes any `.All` permission, `login` saves nothing and leaves an existing `token.json` untouched, `serve` refuses to start (including with `TODO_MCP_START_WITHOUT_TOKEN=1`), every later refresh is refused, and `doctor` reports it. |
| Use a scope beyond To Do | The server requests only Tasks scopes, and every Graph request it makes is under `/me/todo/`, directly or inside `$batch`. It never calls the profile endpoint `GET /me` and never asks for your identity: `openid`, `profile` and `User.Read` are not requested. **Microsoft may still grant more** than was asked, because it returns every permission already consented for your app registration. Any extra Graph permission (typically the portal's default `User.Read`) is named in a warning by `login`, `serve` and `doctor`; `openid`, `profile`, `email` and `offline_access` are ignored. The server never uses an extra permission, but the stored refresh token could obtain tokens that carry it. Remove it from the app registration and revoke the consent. The same applies to `Tasks.ReadWrite` under `TODO_MCP_SCOPE=Tasks.Read`: only the tool list is read-only, and the stored credential can still write. |
| Act as a daemon or without you | It is a public client with no client secret, and `TODO_MCP_CLIENT_SECRET` being set is a startup refusal (`src/config.rs`). App-only access is not configured. Microsoft documents app-only access to To Do writes as `"Not supported."` in any case. |
| Write task content to disk | The task cache (`cache.rs`) lives in memory only and dies with the process. `TODO_MCP_CACHE_TTL_SECONDS=0` keeps no task or list data between tool calls. The `/data` inventory above holds no task content. |
| Put task content in the `serve` log | No line the server logs carries a task title, body, category, checklist item or list name. A list appears in logs only as an opaque `lst_…` hash from `cache::log_ref`, and the log-hygiene test in `tests/tools_read.rs` re-runs its own binary to read the real stderr. See [Known gaps](#known-gaps) for the two exceptions that remain. |
| Accept a Graph token as MCP authorization, or forward the MCP bearer to Graph | Two credentials, never conflated. The inbound bearer is compared in `http.rs` and nowhere else. The Graph access token only ever comes from the token store (`auth/`) and is attached only in `graph/client.rs` (gate 3). |
| Echo a configured value in a configuration refusal | A refusal names the variable and what it accepts, never the value you set, so an ID or secret pasted into the wrong variable does not end up in a log. `src/config.rs` tests assert it. The deliberate exceptions: a file-system error prints the data-directory or bearer-file path it could not use, and an unrecognised command-line argument is named back to you. |

### The ten structural gates

`scripts/gates.sh` runs in `docker build --target test .` and in CI. Each gate backs a
claim this file or the README makes:

| # | Gate | Claim it backs |
|---|---|---|
| 1 | no `/users/{id}` Graph path in `src/` | the server cannot read anyone else's tasks |
| 2 | no `$filter`/`$orderby` under `src/graph/` | all filtering, sorting and searching is client-side |
| 3 | `Authorization` set only in `graph/client.rs` and `http.rs` | the bearer is attached in one place, so the nextLink origin check cannot be bypassed |
| 4 | `with_base` never called from `src/` | shipping code cannot point a client at another host |
| 5 | no `println!`/`eprintln!` outside `cli/out.rs` and `logger.rs` | stdout and stderr each have one owner, so `token` prints the bearer and nothing else |
| 6 | `iana-time-zone` absent from `Cargo.lock` | the host time zone cannot influence a date |
| 7 | `aws-lc-sys` absent from `Cargo.lock` | rustls uses `ring` |
| 8 | exactly one `rustls` in `Cargo.lock` | one TLS stack |
| 9 | `test-fixtures` not a default feature | `with_base` and cleartext transport cannot reach a release build |
| 10 | `from_local_datetime` called only in `domain/datetime.rs` | day-boundary arithmetic has one DST-safe chokepoint |

## Authorization, honestly

- **The static bearer is not MCP-conformant authorization.** MCP authorization is
  optional, and the specification is silent on static bearer tokens: they are out of
  scope, not blessed. This server does not claim conformance. Doing so would put it
  under a SHOULD for OAuth 2.1 and a MUST for RFC 9728 protected resource metadata,
  and v1 ships neither. The bearer is a documented, deliberate deviation.
- **Bind to loopback.** The shipped `compose.yaml` publishes `127.0.0.1:8591` only; the
  explicit host IP is the protection. Inside the container the server binds
  `0.0.0.0:8591`, and that is also the default `TODO_MCP_BIND` on a host run, so set
  `TODO_MCP_BIND=127.0.0.1:8591` when you run the binary outside a container.
- **Put TLS in front if the port is reachable off-host.** The server speaks plain
  HTTP, and a bearer sent over plain HTTP can be intercepted. Use a TLS-terminating
  reverse proxy, and add its hostname to `TODO_MCP_ALLOWED_HOSTS`.
- The `Origin` and `Host` checks defend browsers against DNS rebinding. They are not
  authentication; the bearer is.

## Outbound proxy

Outbound Entra and Graph traffic, which carries the refresh and access tokens inside
TLS, goes through a proxy if any of `ALL_PROXY`, `HTTPS_PROXY` or `HTTP_PROXY` (upper
or lower case) is set in the server's environment. The first one set wins, and
`NO_PROXY` exempts hosts. The shipped `compose.yaml` sets none, but a value in `.env`
applies. The TLS trust store is compiled into the binary, so a proxy cannot read the
traffic unless it holds a certificate that store already trusts.

## Container hardening

The image is `FROM scratch`: one static musl binary for `linux/amd64` and
`linux/arm64`, with no shell, no package manager and no CA bundle or zoneinfo on disk
(both are compiled in). It runs as `USER 65534:65534`. Its healthcheck is the binary
itself (`/todo-mcp healthcheck`), and `/data` is a volume created `65534:65534`, mode
`0700`. That ownership transfers to a named volume only on its first mount while
empty; bind mounts inherit nothing. Both binaries are static non-PIE executables
(`ET_EXEC`, measured on amd64 and arm64), so the executable image itself has no ASLR on
either architecture; the kernel still randomises the stack and mmap regions. That is
accepted, not overlooked; [docs/footprint.md](docs/footprint.md) records the measurement.

The shipped `compose.yaml` adds:

| Key | Effect |
|---|---|
| `ports: "127.0.0.1:8591:8591"` | loopback only. A bare `8591:8591` on Linux writes a Docker iptables rule that bypasses UFW |
| `read_only: true`, `tmpfs: [/tmp]` | read-only root filesystem; only `/data` is writable |
| `cap_drop: [ALL]`, `security_opt: [no-new-privileges:true]` | no capabilities and no privilege escalation |
| `user: "65534:65534"` | the same unprivileged uid as the image |
| `mem_limit: 96m` equal to `memswap_limit: 96m` | swap disabled, so the in-memory access token cannot be paged to disk |
| `pids_limit: 64`, `cpus: 1.0` | resource ceilings |
| `init: true` | the server is not PID 1, so signals are forwarded and delivered |
| `logging: json-file`, `max-size 10m`, `max-file 3` | bounded logs |

A plain `docker run` applies none of the compose keys; add the equivalent flags
yourself. In-memory secrets are zeroed on drop only on a best-effort basis: without
the `zeroize` crate the optimiser may skip it, so erasure is not guaranteed.

## `doctor` and `login` print to stdout, and Docker may keep it

`doctor` prints your list names and, when `TODO_MCP_TZ` is set, one real task's title,
next to its raw and interpreted due date. `login` prints Microsoft's device-code message: the sign-in URL
and a code that can complete the sign-in until it expires. Under `docker compose run`,
the service's `json-file` log driver records that stdout. It stays on the Docker host
until the `--rm` container is removed. If that matters, run `doctor` with the host
binary, or bypass the log driver with the same volume and `.env`:

```bash
docker run --rm --log-driver none --env-file .env \
  -v microsoft-todo-mcp_todo-mcp-state:/data \
  docker.io/hromadkom/microsoft-todo-mcp:latest doctor
```

**Redact the list names and any task title before pasting `doctor` output anywhere.**

## Known gaps

These are known and tracked. Report anything else.

- A panic message is logged with its payload, which could include task content if a
  future bug panics while formatting it.
- When Microsoft refuses the boot refresh, `serve` logs Microsoft's
  `error_description`, which may contain your account name.
- `bearer.token` is not created exclusively. A file that already exists but is empty
  is overwritten in place and keeps its existing mode.
- The server has never run against a live Microsoft account. The scope strings
  Microsoft actually grants to personal and to work accounts, which `auth::vet_grant`
  judges, have not been recorded.

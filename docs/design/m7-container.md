# M7 — The container, made real

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Handoff spec. **M7 is not greenfield.** The `Dockerfile` (5 stages), `compose.yaml`, `ci.yml` and
the `hermetic-gate` action are already written — and **none has ever been built**, because the
Docker daemon was down for the whole of M0. M7 runs them, records what happens, and fixes the few
things only a real build reveals. Do not re-author them; the reasoning behind every key is below.

**Exit gate.** Image ≤ a **measured** gate (`docker build --target size .` at a `SIZE_LIMIT` set
from the first real musl build, never assumed) · `compose up -d` reaches `healthy` under
`read_only: true` + `cap_drop: [ALL]` + `user: "65534:65534"` · **on Linux** a fresh named volume
is `65534:65534` mode `0700` · `docker exec … sh` fails · `SIGTERM` exits **0 within 2 s**.

**Files:** signal registration (`Shutdown`) in `cli/commands.rs`, worker supervision and the
drain loop in `http.rs`, `config.rs`'s `/data` writability probe, `tests/shutdown.rs` — plus the
measured `SIZE_LIMIT` written back into the `Dockerfile` `ARG` only (`ci.yml` passes no
build-arg). **No new container files.**

---

## 1. What already exists, and what M7 owns

| Artefact | State | M7's job |
|---|---|---|
| [`Dockerfile`](../../Dockerfile) — `base`/`test`/`build`/`size`/`release` | written, never built | build all five, set `SIZE_LIMIT` from the result |
| [`compose.yaml`](../../compose.yaml) — hardened `todo-mcp` + dockerized `dev` | written, parser-validated only | bring it up, verify every hardening key took effect |
| [`ci.yml`](../../.github/workflows/ci.yml) + [`hermetic-gate`](../../.github/actions/hermetic-gate/action.yml) | written, never run (no git remote) | close the three gaps in §11 |
| `signal-hook = "0.4"` in [`Cargo.toml`](../../Cargo.toml), `0.4.4` in `Cargo.lock` | dependency present, unused | §2 — the only genuinely new source |
| `config::validate()` `/data` probe | specified in [`m1-auth.md`](m1-auth.md) §5 | §3 — write it |
| `.github/workflows/release.yml` | **absent** | **not M7's.** M8 creates it |

The one check that works with the daemon down, re-run 2026-08-26 on Docker 29.7.2 / Compose
v5.4.0: `docker compose config` round-trips the whole file, exit 0. `mem_limit: 96m` normalises to
`"100663296"`, `cpus: 1.0` to `1`, and every non-`deploy` key (`pids_limit`, `memswap_limit`,
`stop_grace_period`, `init`, `read_only`, `cap_drop`, `security_opt`, `user`, `tmpfs`) survives.

---

## 2. Signals and shutdown — the only real code in this milestone

[calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp) registers `register_conditional_shutdown(sig, 0, always_true)`, whose action is
`low_level::exit(0)` → `libc::_exit(0)` (`signal-hook-0.4.4/src/low_level/mod.rs:54-59`, *"Yes,
the one with underscore. That one doesn't call the at-exit hooks."*). Exit 0, no unwinding, no
destructors — correct for a process whose only state is a memory cache, but here it would truncate
an in-flight tool call, including a Graph **write** whose outcome the client never learns.

Use signal-hook's own double-Ctrl-C idiom. Its doc comment on `register_conditional_shutdown`
(`src/flag.rs:176-180`) is the whole design, verbatim:

> *"On the first run, the flag is `false` and this doesn't terminate. But then the flag is set
> to true during the first run and „arms“ the shutdown on the second run. Note that it matters
> in which order the actions are registered (the shutdown must go first)."*

```rust
const GRACE: Duration = Duration::from_secs(5);      // < compose's stop_grace_period: 10s
let stopping = Arc::new(AtomicBool::new(false));
for sig in [SIGINT, SIGTERM] {
    signal_hook::flag::register_conditional_shutdown(sig, 0, Arc::clone(&stopping))?;  // FIRST, disarmed
    signal_hook::flag::register(sig, Arc::clone(&stopping))?;                          // arms + requests drain
}
unsafe { signal_hook::low_level::register(SIGPIPE, || {})?; }   // a client hanging up must not kill us

while !stopping.load(Ordering::SeqCst) { thread::sleep(Duration::from_millis(100)); }
let deadline = Instant::now() + GRACE;
for _ in 0..WORKERS { server.unblock(); }             // frees exactly ONE thread per call
while Instant::now() < deadline && workers.iter().any(|w| !w.is_finished()) {
    server.unblock(); thread::sleep(Duration::from_millis(25));
}
std::process::exit(0);   // a worker parked in a 30 s Graph call must not outlive the grace
```

**Measured during design** (host prototype, not the container): `kill -TERM` → **exit code 0,
drained in 85 ms**. The 100 ms poll is the floor on an idle server, so the 2 s gate and the 5 s
`GRACE` are not in tension — `GRACE` only elapses when a worker is inside a Graph call.

`unblock()` is not a broadcast. tiny_http 0.12.0 `lib.rs:406-407`: *"Unblock thread stuck in
`recv()` or `incoming_requests()`. If there are several such threads, only one is unblocked."*
Source confirms it — `util/messages_queue.rs:37-41` pushes a single `Control::Unblock` and calls
`notify_one()`. Hence `WORKERS` calls, then nudging until the deadline. The `SIGPIPE` line is
belt-and-braces: Rust's runtime already installs `SIG_IGN` at startup, so a no-op handler behaves
identically. Keep it as a statement of intent — never swap it for one that exits.

> **The supervisor and the drain loop will fight each other unless you tell them apart.**
> [`m2-mcp-http.md`](m2-mcp-http.md) §4 has `main` supervise the workers: a returning `JoinHandle`
> respawns a replacement. A draining worker **also** returns — `pop()` yields `None` → `recv()`
> returns `Err("thread unblocked")` → `incoming_requests()` ends the `for` loop → the closure
> returns normally (a `recv_timeout` loop — m2 §4 names it as a panic site — gets `Ok(None)` and
> is no more distinguishable). `is_finished()` cannot tell the two apart, so a supervisor that
> respawns unconditionally turns the drain into a treadmill: each `unblock()` frees one worker,
> which is instantly replaced, and the process never exits. **The supervisor must check `stopping`
> before respawning**, and the respawn counter must not be tripped by a shutdown.

Hard-exiting at the deadline rather than blocking on `join` is safe for exactly one reason,
recorded in [`m1-auth.md`](m1-auth.md) §6: a refresh interrupted after redemption but before the
durable write leaves a **still-valid old refresh token** on disk, because *"The Microsoft identity
platform doesn't revoke old refresh tokens when used to fetch new access tokens."* The `flock` on
`.token.lock` is released by the kernel on process death regardless. If that ever changes, this
hard exit becomes unsafe along with m1 §6. **Do not register `SIGHUP`** — Docker sends only
`SIGTERM`, and adding SIGHUP invites a stray terminal hangup to kill the daemon.

> **`init: true` looks like cargo-culting for a static binary that forks nothing. It is not.**
> It takes our process *off* PID 1, where the kernel suppresses default signal dispositions — a
> signal with no installed handler is silently discarded. Without `init`, a `SIGTERM` arriving
> before the handlers above are registered (config validation, a device-code wait) is dropped,
> `docker stop` blocks the full `stop_grace_period`, and the container dies by `SIGKILL` with a
> misleading 137. With `init`, docker-init is PID 1 and forwards. Zombie reaping is a side benefit.

---

## 3. `config::validate()` — the `/data` writability probe

Specified in [`m1-auth.md`](m1-auth.md) §5, written in M7 because this is the milestone that can
test it: create and remove `${dir}/.write-probe.<pid>`, and on failure refuse with the named
remediation *"`/data` is not writable by uid 65534 (found owner 0:0, mode 0755). Fix it once
with:* `docker run --rm -u 0 -v todo-mcp-state:/data busybox chown -R 65534:65534 /data`*"* — an
external image, because this one has no shell.

| Subcommand | Runs the probe? |
|---|---|
| `serve`, `login`, `logout`, `token` | yes — refuse to start on failure |
| `doctor` | yes, but **reports as a finding and never refuses**; `doctor` is what a stuck user runs |
| `healthcheck` | **no** |

> **`healthcheck` must not touch `/data`, and must not call `Server::num_connections()`.**
> Two traps, one consequence. The probe runs every 30 s as a fresh process: a `/data` dependency
> turns a permissions regression into "container unhealthy" — the symptom that looks like a dead
> server — instead of the explicit `EACCES` line `serve` prints, and it creates and unlinks a file
> in the token store 2 880 times a day. And `num_connections()` is literally `unimplemented!()` in
> tiny_http 0.12.0 (`lib.rs:374`, confirmed in the vendored source), so calling it panics the one
> probe whose failure looks like a dead container. See [`m2-mcp-http.md`](m2-mcp-http.md) §3.

The `busybox chown` needs two corrections before it is printed anywhere. It restores *ownership*,
not *mode* — `chown -R` leaves the root `0755`, which unblocks the server but fails the `0700`
clause — so append `&& chmod 0700 /data`; see §8's ordering warning. And it names the **unprefixed**
`todo-mcp-state`, which under compose is the wrong volume (§9): the operator-facing form is
`-v microsoft-todo-mcp_todo-mcp-state:/data`. `m1-auth.md` §5 has the uncorrected string; do not
copy it forward unchanged.

---

## 4. Step 0 — preconditions

1. **The Docker daemon is up.** Everything below is blocked on it; nothing here has ever run.
2. **A Linux host for §8 and §9.** macOS VirtioFS remaps ownership and hides exactly the failure
   those steps exist to find; an amd64 image under emulation on an arm64 Mac is not evidence
   either. Verify on the platform you deploy on.
3. **A real `TODO_MCP_CLIENT_ID` in `.env`** — the image ships none. Do **not** log in yet: §8
   wipes the volume to test first-mount ownership, so the order is §8 (`token`) → `login` → §9.
4. Both image pins re-probed live on 2026-08-26 (registry manifest APIs, re-run while writing this):

| Pin | Probe | Result |
|---|---|---|
| `ghcr.io/rust-cross/cargo-zigbuild:0.23.2` | GHCR manifest API, `Accept: …oci.image.index.v1+json` | **200**, an image index carrying `linux/amd64`, `linux/arm64`, `linux/386` |
| `…cargo-zigbuild:0.24.0` | same | **404** — 0.23.2 is still the newest |
| `rust:1.97.1-slim` (the `dev` service) | Docker Hub manifest API | **200** |

The index carries arm64, so the builder runs natively on an Apple-silicon host *and* on an amd64
CI runner. With `--platform=$BUILDPLATFORM` on `base`, **no stage ever runs under QEMU** — `zig cc`
cross-compiles, `ring`'s C and asm included. Multi-platform in one invocation needs a container
builder (`docker buildx create --driver docker-container --use`); the default driver refuses it.

---

## 5. Step 1 — `docker build --target test .`

The hermetic gate, chained with `&&`: `cargo fmt --check`, `cargo clippy --all-targets --locked
-- -D warnings`, `cargo test --locked`, `sh scripts/gates.sh` (nine gates as shipped; ten by the
time M7 runs — [`m5-agenda-timezone.md`](m5-agenda-timezone.md) §3 adds the `from_local_datetime`
chokepoint gate). Worth a one-line
reorder while you are here — the design argued `gates.sh` should run **first**, because a
two-second grep failure beats discovering it after a four-minute clippy run.

| Failure | Cause | Remediation |
|---|---|---|
| exit 137 / SIGKILL while linking test binaries | the Docker VM's memory limit | raise the VM's RAM. `[profile.dev] debug = "line-tables-only"` is already the mitigation — **do not revert it** |
| gate 6/7/8 fail | `Cargo.lock` drifted (`iana-time-zone`, `aws-lc-sys`, a second `rustls`) | a feature was re-enabled; see [`AGENTS.md`](../../AGENTS.md), not the Dockerfile |

> **`.dockerignore` excludes `*.md` while `Cargo.toml` declares `readme = "README.md"`**, so the
> file cargo is told to read is absent from every build context. **Probed on cargo 1.97.1: a
> missing `readme` breaks neither `cargo build` nor `cargo metadata`** — the path is resolved only
> by `cargo package`/`publish`, never run here (`publish = false`). Leave both alone.

> **Nothing in this project ever runs the test suite against the artefact's target.** The `test`
> stage inherits `base`'s `$BUILDPLATFORM` pin, so `cargo test` compiles for the *builder's* glibc
> triple — aarch64 on a Mac, amd64 in CI — while the shipping binary is static
> `*-unknown-linux-musl`. The **first execution of musl-linked code in this project's history is
> `docker run` on the release image**, making §7's smoke the only musl test that exists. If a
> worker stack-overflows only there, that is why: musl's default thread stack is a fraction of
> glibc's 8 MiB, and `http.rs`'s explicit `stack_size(512 * 1024)`
> ([`m2-mcp-http.md`](m2-mcp-http.md) §3) is the knob that makes it a non-event.

---

## 6. Step 2 — `docker build --target size .`, and the number

Run `docker build --target size --platform linux/amd64 .`, then again for `linux/arm64`. `FROM
build AS size` inherits `base`'s `$BUILDPLATFORM` pin, so both `stat` runs execute natively on a
cross-compiled binary, printing `todo-mcp: <n> bytes (limit <limit>)`.

**Record both numbers. They are the first honest figures this project has.** The ~336 KB host
release binary quoted in `AGENTS.md` is meaningless — LTO dead-strips rustls, ureq, tiny_http and
chrono-tz because M0 calls none of them. The measured floor for the linked dependency set is
**~2.6 MB (macOS Mach-O)** and static musl will be larger. `SIZE_LIMIT` sits at a deliberately
generous **8 MiB (`8388608`)**; set it from the larger arch plus headroom, then ratchet down. The
numbers land in `docs/footprint.md` — **created in M8**, not here.

> **`SIZE_LIMIT` is written in two places and they can drift silently.** `ARG SIZE_LIMIT=8388608`
> in the `Dockerfile` and `build-args: SIZE_LIMIT=8388608` in `ci.yml`. The CI value wins in CI,
> the ARG default wins locally, so a local build can pass a gate CI would fail and vice versa.
> Delete the `build-args` line and let the ARG default be the single source of truth.

The binary is already stripped (`[profile.release] strip = true`). Once §7 has built the release
image, extract it and settle the ASLR claim in the Dockerfile header — no toolchain needed,
`e_type` is the two little-endian bytes at ELF offset 16:

```
id=$(docker create microsoft-todo-mcp:latest); docker cp "$id":/todo-mcp /tmp/todo-mcp; docker rm "$id"
od -An -tx1 -j16 -N2 /tmp/todo-mcp        # 03 00 = ET_DYN (static-pie) · 02 00 = ET_EXEC (non-PIE)
```

Expect **`02 00` on arm64**: `aarch64-unknown-linux-musl` produces a static **non-PIE**, because
rustc sets `static_position_independent_executables` only for x86_64 and s390x. **There is no
image-level ASLR on arm64.** State it in `docs/footprint.md`; it is accepted, not overlooked.

---

## 7. Step 3 — `docker build --target release .`, inspected

`docker build --target release -t microsoft-todo-mcp:latest .`, then:

| Check | Command (`docker image inspect --format …`) | Expected |
|---|---|---|
| user | `{{.Config.User}}` | `65534:65534` |
| volume | `{{json .Config.Volumes}}` | `{"/data":{}}` |
| healthcheck | `{{json .Config.Healthcheck}}` | `["CMD","/todo-mcp","healthcheck"]`, 30s/5s/5s/3 |
| entrypoint | `{{json .Config.Entrypoint}} {{json .Config.Cmd}}` | `["/todo-mcp"] ["serve"]` |
| port | `{{json .Config.ExposedPorts}}` | `{"8591/tcp":{}}` |
| env | `{{json .Config.Env}}` | `TODO_MCP_DATA_DIR=/data`, `TODO_MCP_BIND=0.0.0.0:8591`, and **no `TODO_MCP_CLIENT_ID`** |
| size | `{{.Size}}` | ≈ the §6 binary; much larger means a stage leaked in |
| no shell | `docker run --rm --entrypoint sh …:latest -c true`, and `docker exec -u 0 <ctr> sh` | both **fail** — `executable file not found` |

`FROM scratch` is the right base on its honest merits: a distroless static base buys
`/etc/passwd`, `/tmp`, `/etc/ssl/certs` and nsswitch, and this build uses **none** of them — a
numeric `USER` needs no passwd entry, the atomic token write uses a tmp file **inside `/data`**
(rename must be same-filesystem anyway), ureq's `rustls` feature compiles in the Mozilla root
store via `webpki-roots`, chrono-tz compiles the tzdb in, static musl needs no libc — and it does
**not** solve the `/data` ownership problem, since neither base can `RUN chown` in the final stage.
The root store is source-verified in ureq 3.4.0 (`impl Default for TlsConfig { root_certs:
RootCerts::WebPki }`; `src/tls/rustls.rs:195` builds it from `webpki_roots::TLS_SERVER_ROOTS`), so
**never enable the opt-in `platform-verifier` feature** — it reaches for a system trust store
scratch does not have. The first `docker run` also settles `getrandom 0.2` on scratch, which
`Cargo.toml` flags as a *"`/dev/urandom` code path that FROM scratch may not provide"*: if §9's
`token` generates a bearer, the CSPRNG works.

---

## 8. Step 4 — `/data` ownership, on Linux

**Settled in theory; this step is the empirical half. Do not re-open it as a disagreement.** From
moby/containerd source, carried in full in [`m1-auth.md`](m1-auth.md) §5: `populateVolume()`
returns early when the image lacks the path (leaving `root:root 0755`); when it **does** exist,
`copyExistingContents` → `fs.CopyDir` chmods and `Lchown`s the volume root from the image
directory **before copying any entries**, so ownership *and* mode transfer even from an empty one.
`COPY --from=build --chown=65534:65534 --chmod=0700 /data /data` is what makes the path exist.
Required: a **volume** mount (bind mounts excluded outright), the **first** mount, an **empty** volume.

`--chmod` is load-bearing, and was a source-verified fix during handoff: without it BuildKit
creates the destination via `MkdirAll(target, defaultDirectoryMode=0755, chown)` and the builder's
`chmod 0700` is silently discarded — image and volume land at `0755` and fail the `0700` clause.
**Do not "simplify" it away.** Likewise the UID `65534`, in both `Dockerfile` and `compose.yaml`:
a wrong UID in a `chown` produces exactly the `EACCES` this design exists to pre-empt.

```bash
docker volume rm microsoft-todo-mcp_todo-mcp-state          # ensure empty/absent
docker compose run --rm -T todo-mcp token > /dev/null       # FIRST mount, and a real write as 65534
sudo stat -c '%u:%g %a' "$(docker volume inspect -f '{{.Mountpoint}}' microsoft-todo-mcp_todo-mcp-state)"
# expect: 65534:65534 700
```

`token` is the ideal first-mount probe *if it writes at all*: no Entra round trip, and a bearer
persisted under `/data` mode 0600 as uid 65534 under `read_only: true` is the whole failure mode
end to end. **Do not assume a filename.** Whether the inbound MCP bearer is persisted, and under
what name, is unsettled — neither [`m1-auth.md`](m1-auth.md) §5 nor
[`m2-mcp-http.md`](m2-mcp-http.md) §3 says, and [`m8-docs-release.md`](m8-docs-release.md) §7 lists
it as an open question whose answer `SECURITY.md`'s `/data` inventory depends on. Settle it here,
because M7 is the milestone that can watch the write happen. If `token` turns out to generate
without persisting, the first-mount probe is `login` instead — at the cost of doing §8 and the
sign-in in one step.

> **Do not let your inspection be the first mount.** `docker run --rm -v vol:/data busybox stat …`
> is the obvious check, and it destroys what it measures: busybox has no `/data`, so
> `populateVolume()` returns early, the volume is created `root:root 0755` and is now non-empty as
> far as the *next* mount is concerned — our image never populates it and you "reproduce" the bug
> you were testing for. Same trap for running the `busybox chown` preventively. Read the volume
> from the host as above, or inspect only **after** the real image has mounted it.

Remediations, in preference order: (1) `docker volume rm` and retry — the documented reset; (2)
the `busybox chown` from §3 plus `chmod 0700`; (3) bind-mount a host directory the invoking user
owns and run as that user: `-v "$HOME/.todo-mcp:/data" --user "$(id -u):$(id -g)"` (Linux; a
bind mount does not change the container's uid 65534), sidestepping the volume driver.
**UNVERIFIED:** SELinux-enforcing hosts and Podman — named volumes are relabelled by the daemon and
bind mounts are not, so (3) may need `:Z`; `no-new-privileges` there is untested too.

---

## 9. Step 5 — `compose up -d`, hardened

> **The documented login command writes to a different volume than the one compose mounts.**
> [`README.md`](../../README.md) step 1 and the header of [`compose.yaml`](../../compose.yaml) both
> say `docker run --rm -it -v todo-mcp-state:/data … login`. Compose namespaces its volumes by
> project — verified live on 2026-08-26: `docker compose config` resolves `todo-mcp-state` to
> **`microsoft-todo-mcp_todo-mcp-state`**, and `compose.yaml`'s own reset comment uses that
> prefixed name. `docker run` applies no prefix, so the refresh token lands in a volume the server
> never mounts. The symptom is not a permissions error — it is `serve` refusing to start as if no
> login had happened. **Pick one fix and make all three call sites agree:** an explicit
> `name: todo-mcp-state` under `volumes:` (one line, and the README's published-image path keeps
> working for a reader who never clones the repo), the prefixed name in both documented
> `docker run` commands, or `docker compose run --rm todo-mcp login` instead of them.

> **`serve` refusing to start + `restart: unless-stopped` = a silent crash loop.**
> [`m2-mcp-http.md`](m2-mcp-http.md) §6 makes `serve` refuse without a usable token — the one
> refuse-to-start that is correct, because the tool list is frozen at boot and a later `login`
> would otherwise leave the write tools permanently invisible. Under `restart: unless-stopped`
> that refusal becomes an endless restart: `docker compose ps` shows `restarting` with no cause,
> the healthcheck never gets far enough to report anything, and the message is in
> `docker compose logs todo-mcp` and nowhere else. Hence the order **`token` → `login` → `up -d`**
> — a first-run experience worth walking through deliberately.

Once up, with `C=$(docker compose ps -q todo-mcp)` — every row below is `docker inspect --format '…' "$C"`:

| Check | Format string | Expected |
|---|---|---|
| hardening took effect | `{{.HostConfig.ReadonlyRootfs}} {{json .HostConfig.CapDrop}} {{json .HostConfig.SecurityOpt}} {{.Config.User}}` | `true ["ALL"] ["no-new-privileges:true"] 65534:65534` |
| swap disabled | `{{.HostConfig.Memory}} {{.HostConfig.MemorySwap}}` | `100663296 100663296` — equal is what disables swap, so the in-memory access token cannot be paged to host disk |
| loopback only | `{{json .NetworkSettings.Ports}}` | `HostIp: "127.0.0.1"`. The prefix is the actual protection: a bare `"8591:8591"` on Linux writes a DOCKER-chain iptables rule that **bypasses UFW** |
| healthy | `{{.State.Health.Status}}`, `{{json .State.Health.Log}}` | `healthy` within `start_period` + one `interval`; `ExitCode: 0` |
| no shell | — (run `docker compose exec todo-mcp sh` instead) | fails, `executable file not found` |

The healthcheck is the binary itself because scratch has no shell and no curl: a raw `TcpStream`
`GET /healthz`, unauthenticated, provably Graph-free. Point it at `127.0.0.1:<port>` derived from
the configured bind's **port**, never the bind address verbatim — Linux happens to map a connect
to `0.0.0.0` onto loopback, and depending on that is a portability bet with no upside.

**Settled — the unhealthy exit code.** The Dockerfile reference documents 0 as healthy and 1 as
unhealthy and reserves 2. `healthcheck` returns `cli::exit::UNHEALTHY` = **1** for every failure,
invalid configuration included, so it never emits the reserved code.

Know rather than change: `tmpfs: [/tmp]` is inert (the atomic write uses `/data`) but has no
`size=`, and tmpfs pages count against `mem_limit: 96m` — cap it if anything ever writes there.
And `docker compose down -v` deletes the token: `docs/operations.md` (M8), in bold.

---

## 10. Step 6 — SIGTERM, SIGINT, and the 2 s clause

`time docker compose stop todo-mcp` (SIGTERM, then SIGKILL at `stop_grace_period: 10s`), then
`docker inspect --format '{{.State.ExitCode}} {{.State.OOMKilled}}' "$C"`:

| Observation | Meaning |
|---|---|
| exit **0**, well under 2 s | the drain worked; docker-init propagated the child's status |
| exit **137**, ~10 s | `SIGKILL` at the grace deadline — handlers never registered, or §2's supervisor treadmill |
| exit **143** | `SIGTERM` reached a process with the default disposition — the drain loop is not running |

Bring it back with `compose up -d`, then pin by hand: two quick `docker kill -s TERM "$C"` — the
second exits immediately, the conditional shutdown being armed by then. Expect it to come **back**:
`restart: unless-stopped` restarts on exit 0 too; only `compose stop` marks it stopped. The rest is
`tests/shutdown.rs` (~120 LOC), covering only the in-process half — docker-init is this step. Ports
parsed out of the stderr log line as `http_smoke.rs` does; `env_clear()` throughout.

| # | Test | Assertion |
|---|---|---|
| 1 | SIGTERM to a live `serve` subprocess | exits **0** within 2 s |
| 2 | SIGTERM during an in-flight request | the response is written before exit |
| 3 | second SIGTERM during a drain | exits immediately, code 0 |
| 4 | a worker returning normally during shutdown | supervisor does **not** respawn (§2's treadmill) |
| 5 | `/data` probe with a read-only dir | `validate()` refuses with the remediation string; `doctor` reports and continues |

---

## 11. CI, and the three gaps a first real run exposes

Structure is sound: a composite action rather than a reusable workflow (a `workflow_call` job
reports as `<caller> / <called>` and would rename the required status checks), the `test` stage as
the single definition of "does this tree pass?", and a `TZ=Pacific/Kiritimati` re-run of the
already-compiled binaries (`--offline`, ~1 s). Three gaps only a real run exposes:

| Gap | Evidence | Fix |
|---|---|---|
| **`scope=release` has no producer** | `hermetic-gate` writes `cache-to: …scope=test`; `release-build` has `cache-from: type=gha,scope=release` and **no `cache-to` anywhere** | mirror the gate's rule — write the release scope on `main` only |
| **`cancel-in-progress: true` is unconditional** | `ci.yml` concurrency block | a rapid second push to `main` cancels the run that was producing the cache; gate it to `github.event_name == 'pull_request'` |
| **The `release` stage is never built in CI** | `release-build` targets `size`, and nothing targets `release` | `USER`, `VOLUME`, `HEALTHCHECK` and the `COPY --chmod` are unexercised; add a `--target release` build for both arches |

Job names as shipped are **`Hermetic gate`** and **`Release image`** (the latter builds `release`
for both arches through the size gate, then smoke-tests the amd64 image on Linux); use those exact
strings when the branch ruleset is created, which happens after the first release (M8). `release.yml`,
`dependabot.yml` and the tag ↔ `Cargo.toml` check are M8's: [`m8-docs-release.md`](m8-docs-release.md).

---

## 12. Settle during M7

- **`/data` ownership on Linux** — theory settled (§8), measurement not. Unresolved since
  [`m1-auth.md`](m1-auth.md) §5 / [`m2-mcp-http.md`](m2-mcp-http.md) §11, and the likeliest
  "works on my machine, fails in compose" failure in the project.
- **Real musl binary size, both arches** — sets `SIZE_LIMIT`; lands in `docs/footprint.md` (M8),
  with idle RSS measured at the same time (`docker stats --no-stream`). [calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp) ships ~4 MB
  image / ~3 MB idle RSS; this project has no comparable figure.
- **Healthcheck exit code** — settled: 1 for every failure (§9).
- **Where the inbound MCP bearer is persisted** (§8) — no shipped file, `m1-auth.md` §5 or
  `m2-mcp-http.md` §3 names one, and `SECURITY.md`'s `/data` inventory
  ([`m8-docs-release.md`](m8-docs-release.md) §7) is wrong if it is guessed. M7 is the first
  milestone that can watch `token` run under `read_only: true` and see what lands.
- **The volume-name mismatch** (§9) — M7 picks one of the three fixes, because §9 cannot run
  until it does; [`m8-docs-release.md`](m8-docs-release.md) §2 and §12 apply the choice to
  `README.md`, `compose.yaml`, `Dockerfile:60` and `docs/operations.md`, which are M8's files, not
  M7's.
- **Whether 512 KiB worker stacks survive a rustls handshake on musl** — not exercised until the
  release image runs (§5).
- **SELinux / Podman** — bind-mount labelling and `no-new-privileges` behaviour untested (§8).

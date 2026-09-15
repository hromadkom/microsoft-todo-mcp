# M1 — Entra device-code auth

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Handoff spec. Everything here was verified against Microsoft Learn or by **live
probes against the real endpoints** during design; where something is unverified it
says so explicitly. Do not re-derive it.

**Exit gate.** `login` completes against a real personal MSA and writes
`token.json` mode 0600 in a 0700 dir · `doctor` prints granted scopes *read back
from the token response*, absolute `expires_at`, and `/data` owner+mode · a hidden
`debug-lists` prints real list names and identifies `defaultList` · a `login` under
a running `serve` **wins**, and the server adopts it · `strings token.json | grep
eyJ` finds nothing · every AADSTS code renders a named remediation.

**Files:** `config.rs`, `auth/{mod,entra,device_code,aadsts,store}.rs`, a minimal
`graph/client.rs`, and the `login`/`logout`/`token`/`doctor` subcommands.

---

## 1. The three HTTP calls

Public client throughout: **no `client_secret`, no `redirect_uri`, no
`response_type`** on any leg.

### 1a. Device authorization

```
POST https://login.microsoftonline.com/common/oauth2/v2.0/devicecode
Content-Type: application/x-www-form-urlencoded

client_id=<TODO_MCP_CLIENT_ID>
&scope=https%3A%2F%2Fgraph.microsoft.com%2FTasks.ReadWrite+offline_access
```

Two form fields only. **Scope settled by live probe, not by reading:**

| Sent | Result |
|---|---|
| `https://graph.microsoft.com/Tasks.ReadWrite offline_access` | **200** ← use this |
| `Tasks.ReadWrite offline_access` | 200 (short name resolves against an implicit resource) |
| `…/Tasks.NotAReal offline_access` | 400 `invalid_scope` AADSTS70011 |
| `…/Tasks.ReadWrite …/.default` | 400 — `.default` is incompatible with any other scope |

Use the **fully-qualified** form; it is unambiguous and is what the `granted_scope`
comparison in §4 must cope with anyway. `offline_access` is **mandatory** — Learn:
`refresh_token` is *"Only provided if `offline_access` scope was requested."*

`/devicecode` validates scopes eagerly, so a typo'd `TODO_MCP_SCOPE` fails in
~200 ms instead of after a 15-minute human round trip.

```rust
#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: Secret,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    #[serde(default = "default_interval")] pub interval: u64,
    pub message: String,
}
fn default_interval() -> u64 { 5 }  // RFC 8628 §3.2: clients MUST default to 5
```

Do **not** declare `verification_uri_complete`. Learn: *"The
`verification_uri_complete` response field is not included or supported at this
time."*

### 1b. Redemption (the poll)

```
POST …/common/oauth2/v2.0/token
grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code
&client_id=<…>&device_code=<…>
```

Three fields. **No `scope` on this leg.**

### 1c. Refresh

```
POST …/common/oauth2/v2.0/token
grant_type=refresh_token&client_id=<…>&refresh_token=<…>
&scope=https%3A%2F%2Fgraph.microsoft.com%2FTasks.ReadWrite+offline_access
```

**Send the `requested_scope` stored on disk, not the value in config.** If
`cfg.scope != stored.requested_scope`, refuse with a named error telling the user to
re-run `login`. Otherwise someone editing `TODO_MCP_SCOPE` from `Tasks.Read` to
`Tasks.ReadWrite` gets a permanently read-only server with no explanation.

### 1d. Wire structs

```rust
#[derive(Debug, Deserialize)]
pub struct TokenSuccess {
    pub access_token: Secret,
    pub token_type: String,                              // always "Bearer"
    pub expires_in: i64,
    #[serde(default)] pub scope: Option<String>,         // GRANTED; see §4
    #[serde(default)] pub refresh_token: Option<Secret>, // only with offline_access
}
// id_token, client_info, ext_expires_in, foci deliberately NOT declared —
// serde ignores unknown fields, and declaring them invites someone to log them.

#[derive(Debug, Deserialize)]
pub struct TokenErrorBody {
    pub error: String,
    #[serde(default)] pub error_description: Option<String>,
    #[serde(default)] pub error_codes: Vec<i64>,
    #[serde(default)] pub suberror: Option<String>,
    #[serde(default)] pub trace_id: Option<String>,
    #[serde(default)] pub correlation_id: Option<String>,
}
```

`suberror`/`error_uri` are `Option` because they appear inconsistently — live
`authorization_pending` carried `error_uri`, live `AADSTS7000014` did not.

**ureq call:** `send_form(iter)` sets the content type and URL-encodes. Read the
body with `res.body_mut().with_config().limit(256 * 1024).read_to_vec()` — bounds a
hostile authority.

**Redaction enforced by types.** `Secret(String)` with manual `Debug`/`Display`
printing `<redacted len=N>`. `error_description` is safe to print but must be
truncated at the first `Trace ID:`:

```rust
/// "AADSTS<n>: <sentence>\r\nTrace ID: …\r\nCorrelation ID: …"
/// Live responses sometimes use spaces instead of CRLF — split on the marker.
pub fn short_description(d: &str) -> &str {
    d.split("Trace ID:").next().unwrap_or(d).trim().trim_end_matches(['\r','\n',' '])
}
```

---

## 2. The polling loop

Microsoft's error table lists exactly four codes — `authorization_pending`,
`authorization_declined`, `bad_verification_code`, `expired_token` — and **omits
`slow_down`**. RFC 8628 §3.5 requires handling it. Six back-to-back live polls
produced only `authorization_pending`, so the `slow_down` branch is **defensive and
untested against the real service**. Handle it anyway: `interval += 5`, cap 60 s.

Inject `sleep` and `now` so tests don't take 15 minutes.

### The verification URI: render verbatim

Print the server's own `message` field **unmodified**. Do not construct a URL, do
not swap in a "known" one, and **do not assert on a specific value in a test** —
that would be a time bomb. Values observed during design already differ from every
widely-cited constant (`/common` returned `https://login.microsoft.com/device`,
`/consumers` returned `https://www.microsoft.com/link`).

---

## 3. AADSTS handling

`aadsts.rs` is a **pure function**: `code -> Diagnosis { summary, remediation,
terminal, delete_token }`. No I/O, so it is exhaustively testable.

**Delete `token.json` only on `{530036, 700082, 70008}`.** Never on a bare
`invalid_grant`: a malformed refresh request returns `invalid_grant` +
`AADSTS9002313` *"Invalid request. Request is malformed or invalid."*, and deleting
a good token on that is a self-inflicted outage.

The user-facing catalogue lives in
[`docs/app-registration.md`](../app-registration.md#troubleshooting). Codes to
implement: 7000218 (public-client toggle — the #1 setup failure), 50194
(single-tenant vs `/common`), 90094 (admin consent required — how a non-admin
discovers they are blocked), 65001, 530036, 700016, 50105, 7000112, 70011,
70018/70019/70020, 65004, 9002313, 70008, 7000215/7000222, 900023, 500011.

Wire format printed by `login`/`doctor`:

```
error: <one-line summary>
  Microsoft said: AADSTS<n>: <short_description>
  Fix: <remediation>
  Trace ID: <…>  Correlation ID: <…>
```

> **Unverified:** which leg `7000218` fires on (`/devicecode` or `/token`). Design
> could not create a non-public-client registration to test. **Run the catalogue on
> both legs.** An implementer with a spare tenant should confirm and simplify.

---

## 4. Scope readback — two traps

The `scope` field **is** returned and **does** list granted scopes. But:

1. **Encoding is inconsistent.** Learn's device-code sample shows
   `"scope": "User.Read profile openid email"`; the auth-code/refresh sample shows
   `"scope": "https%3A%2F%2Fgraph.microsoft.com%2Fmail.read"` — **percent-encoded**.
   A naive `granted.contains("Tasks.ReadWrite")` fails on the encoded form *and* on
   case (`mail.read` vs `Mail.Read`).
2. **It is documented optional.** *"Optional. This parameter is non-standard and,
   if omitted, the token is for the scopes requested on the initial leg."* If
   absent, **fall back to the requested scope** — otherwise a missing field
   silently unregisters every write tool.

```rust
/// Tolerates percent-encoding, the fully-qualified resource-URI form, and case.
pub fn scope_granted(granted: &str, want: &str) -> bool {
    granted.split_whitespace().any(|tok| {
        let t = percent_decode_lossy(tok);              // ~15 lines, no new crate
        t.rsplit('/').next().unwrap_or(&t).eq_ignore_ascii_case(want)
    })
}
/// Absent `scope` means "we got what we asked for".
pub fn effective_scope(r: &TokenSuccess, requested: &str) -> String {
    r.scope.clone().unwrap_or_else(|| requested.to_string())
}
```

Assert at startup that the effective scope grants `Tasks.Read` **or**
`Tasks.ReadWrite`, and refuse otherwise — a token with neither is a
misconfiguration, not a read-only mode.

> **As implemented, this is `auth::vet_grant`**, the single scope policy that `login` and
> every refresh call before anything is written. It refuses any granted `.All`
> permission, refuses a grant carrying neither Tasks permission, and caps the rest at
> `TODO_MCP_SCOPE` — a ceiling, because Entra returns every scope already consented, not
> only the one requested. Other extra Graph permissions (typically `User.Read`), and
> `Tasks.ReadWrite` granted under a `Tasks.Read` config, produce a warning; `openid`/`profile`/`email`/`offline_access` are ignored. The absent-`scope`
> fallback above still applies, so a `.All` that Microsoft does not report cannot be seen.

### `expires_at`

Absolute RFC3339 computed from `expires_in`. **Never hard-code 3600** — Entra
randomises 60–90 min. Refresh proactively at `expires_at − 300 s`.

### `account_id`

**The literal string `"default"` in v1.** There is no derivation path: `openid`/
`profile` are not requested and `/me` is never called. `todo_account_status` reports
`identity: "not requested (no openid scope)"`. If a real opaque id is ever wanted,
the one mechanism is `client_info=1` on the token request, storing `uid.utid` —
deferred, not invented now.

---

## 5. The token file

```json
{
  "schema_version": 1,
  "account_id": "default",
  "client_id": "…",
  "authority": "https://login.microsoftonline.com/common",
  "requested_scope": "https://graph.microsoft.com/Tasks.ReadWrite offline_access",
  "granted_scope":  "https://graph.microsoft.com/Tasks.ReadWrite offline_access",
  "refresh_token": "0.AXoA…",
  "obtained_at": "2026-08-25T13:49:05.113Z",
  "obtained_by": "device_code"
}
```

**The access token is deliberately not persisted.** It keeps the exit criterion
`strings token.json | grep eyJ` *meaningful* — persist a work-account AT and that
test fails by construction, since work ATs are JWTs starting `eyJ`. Refresh-token
prefixes (`0.A…` work, `M.C5…` MSA) don't collide, so the test stays honest. The RT
in the same file is strictly more powerful anyway; the cost is one ~200 ms refresh
at start.

`obtained_at` needs **sub-second precision** — it is the compare-and-swap key, and
two writes inside the same second is a real race in tests.

### Permissions, and the trap nobody documents

- `token.json` → `create_new(true).mode(0o600)` so the mode is set **at creation**,
  never by a follow-up `set_permissions` (which leaves a window).
- Data dir → `0o700`. If `mode & 0o077 != 0`, **WARN with the octal mode and the
  exact `chmod 700`** — do not silently chmod someone's bind mount.

> **The trap, and why the Dockerfile avoids it.** A Docker named volume mounted at a
> path the image does **not** contain is created **root:root 0755** by the local
> volume driver — there is no ownership to copy, and a process running as 65534 under
> `read_only: true` then cannot create `token.json`, surfacing as a bare `EACCES`.
> On a `FROM scratch` image that is the default outcome.
>
> The `Dockerfile` sidesteps it by making the path exist:
> `COPY --from=build --chown=65534:65534 --chmod=0700 /data /data`. Settled from
> moby/containerd source — `populateVolume()` returns early only when the image path
> is absent; when it exists, `copyExistingContents` → `fs.CopyDir` chmods and
> `Lchown`s the volume root **from the image directory, before copying any entries**.
> So ownership *and* mode transfer even from an empty directory.
>
> Three conditions, all required: a **volume** mount (bind mounts are excluded
> outright), the **first** mount, and an **empty** volume. `docker volume rm` is the
> documented reset.
>
> `--chmod` is load-bearing: without it BuildKit creates the destination via
> `MkdirAll(target, 0755, chown)` and the builder's `chmod 0700` is discarded.
>
> Still verify on Linux — macOS VirtioFS remaps ownership and hides any failure.

Handling, in order:

1. `config::validate()` probes writability by creating and removing
   `${dir}/.write-probe.<pid>`, and refuses to start with:
   *"`/data` is not writable by uid 65534 (found owner 0:0, mode 0755). Fix it once
   with:* `docker run --rm -u 0 -v todo-mcp-state:/data busybox chown -R 65534:65534 /data`*"*
   — an external image is required because this one has no shell.
2. Document the no-chown alternative: bind-mount a host dir the invoking user owns
   and run as that user, `-v "$HOME/.todo-mcp:/data" --user "$(id -u):$(id -g)"` (Linux;
   a bind mount does not change the container's uid 65534).
3. `doctor` prints resolved dir, owner uid:gid, octal mode, and the token file mode.

### Atomic write

The lock goes on a **separate `.token.lock` file, never on `token.json`.**
`rename()` replaces the inode, so a lock held on the *old* inode protects nothing —
locking the data file is a correctness bug that only appears under contention.

```rust
pub fn save_atomic(dir: &Path, new: &TokenFile, base: Option<&TokenFile>)
    -> Result<SaveOutcome, StoreError>
{
    let lock = OpenOptions::new().create(true).read(true).write(true)
        .mode(0o600).open(dir.join(".token.lock"))?;
    lock.lock()?;                                   // std, stable since 1.89 (flock LOCK_EX)

    if let (Some(base), Ok(disk)) = (base, load_locked(dir)) {
        if disk.obtained_at > base.obtained_at {
            return Ok(SaveOutcome::Adopted(Box::new(disk)));
        }
    }
    let tmp = dir.join(format!("token.json.tmp.{}.{}", std::process::id(), nanos()));
    { let mut f = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
      f.write_all(&serde_json::to_vec_pretty(new)?)?; f.write_all(b"\n")?; f.sync_all()?; }
    fs::rename(&tmp, dir.join("token.json"))?;      // atomic within one filesystem
    File::open(dir)?.sync_all()?;                   // fsync the DIRECTORY — the rename itself
    Ok(SaveOutcome::Wrote)
}                                                   // flock released on drop
```

`flock` is per-open-file-description, so two `File::open` calls **in the same
process** genuinely contend. That is what makes the in-process tests real — and it
means the lock must be acquired in exactly one function or you self-deadlock.

The tmp name carries pid + nanos so `create_new` never collides with a stale tmp
from a crash; a stale tmp is inert (only `logout` sweeps it).

### `SaveMode` — login must win

```rust
pub enum SaveMode { Refresh, Login }
```

The CAS above is correct for **refresh**. It is **wrong for `login`**: a user who
just typed a device code must win unconditionally. `SaveMode::Login` skips the adopt
branch entirely and writes regardless.

---

## 6. The refresh sequence

The flock is **released across the network call**. Holding it would make adoption
impossible once a refresh starts, and a 20 s stall would block `login` entirely.

> lock → re-read → **unlock** → refresh over the network → **re-lock → re-read →
> compare `obtained_at` → write or adopt** → unlock

On re-entry, if the on-disk `obtained_at` is strictly newer than the copy we
started from, **discard our freshly minted token and adopt the disk one.** We lose
one access token and keep the user's live chain.

This is safe *only* because of the verified Microsoft behaviour: *"The Microsoft
identity platform doesn't revoke old refresh tokens when used to fetch new access
tokens."* Both chains survive; adoption is what bounds the fan-out. **If that ever
changes, this design breaks** and the hold-across-refresh variant becomes mandatory.

Single-flight in-process is a `Mutex<Option<Cached>>` that re-checks under the lock.

```rust
pub trait TokenEndpoint: Send + Sync {
    fn redeem_refresh_token(&self, rt: &str, scope: &str) -> Result<TokenSuccess, AuthError>;
}
pub struct TokenProvider<E: TokenEndpoint> { /* endpoint, dir, cfg, cached */ }
impl<E: TokenEndpoint> TokenProvider<E> {
    pub fn access_token(&self) -> Result<Secret, AuthError>;  // the ONLY thing graph/ calls
}
```

That trait is also the test seam — no network is touched in tests.

---

## 7. Tests — `tests/token_store.rs`

Plain `#[test]` + `std::thread`, no runtime. The stub records every `rt` it is
called with, counts calls, and blocks on a `Barrier` so interleavings are
deterministic rather than sleep-and-pray.

| # | Test | Assertion |
|---|---|---|
| 1 | login under a running refresh is adopted | disk ends with `RT-NEW`/`device_code`; the refresher's `save_atomic` returned `Adopted`; next `access_token()` uses `RT-NEW` |
| 2 | 10 concurrent callers → **exactly one** refresh | `stub.calls() == 1`, all 10 get the identical string |
| 3 | stale tmp file is harmless | `load()` still returns `RT-OLD`; `save_atomic` succeeds; stale tmp untouched |
| 4 | future schema refused, never overwritten | `Err(FutureSchema{found:99,supported:1})`; file **byte-identical** before and after |
| 5 | corrupt file reported, not deleted | `Err(Corrupt)`, bytes unchanged, `access_token()` → `AuthRequired` so the server still starts |
| 6 | wrong `client_id` refused | `Err(WrongApp)`, **no** network call reached the stub |
| 7 | scope change forces re-login | `Err(ScopeChanged)`, stub never called |
| 8 | two savers never produce a torn read | 2×200 writes with a concurrent reader; every read parses |

Plus `config_refusals.rs` (every `validate()` rule, and **none echoes a value**) and
`secret_leak.rs` (no token substring reaches stderr or a tool result).

---

## 8. Settle before/while writing this

- **`7000218` leg** — run the catalogue on both `/devicecode` and `/token`.
- **`slow_down`** — branch is untested against the real service.
- **`/data` ownership on a scratch image + named volume** — theory settled in §5 from
  moby/containerd source; the empirical half is unrun. Verify on **Linux**
  ([`m7-container.md`](m7-container.md) §8). This is the most likely "works on my
  machine, fails in compose" failure in the project.
- **UID consistency** — `Dockerfile` and `compose.yaml` both use **65534**; the wrong
  uid in a chown instruction produces exactly the EACCES this design pre-empts.
- **`Secret` zeroing on drop** is best-effort without the `zeroize` crate. Say so in
  the threat model rather than implying guaranteed erasure.

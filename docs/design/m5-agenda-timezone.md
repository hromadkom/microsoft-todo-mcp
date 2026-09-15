# M5 — `todo_agenda` and timezone

> **Historical pre-implementation spec.** Code, [README](../../README.md) and [SECURITY.md](../../SECURITY.md) are authoritative; UNVERIFIED items may since be settled.

Handoff spec. Timezone is **the highest-risk semantic in the project**: a To Do due date is
an all-day date that Graph stores as an instant, so the naive reading is wrong by one day
for much of the planet for part of every day. The arithmetic below was run, not reasoned
about, and the `LocalResult::None` dates are reproduced measurements. Do not re-derive it;
§12 collects what is still open.

**Exit gate.** *"Plan my day"* in **exactly one** tool call with all six sections · the same
fixture under `Pacific/Kiritimati` vs `America/Los_Angeles` produces **different** buckets ·
the DST fixtures pass — `America/Santiago` 2026-09-06, `Asia/Beirut` 2026-03-29,
`America/Havana` 2026-03-08 (local midnight does not exist) and the fall-back ambiguous
hour · the 2026-12-31T23:30Z year-boundary case passes for **both** `Europe/Prague`
(`due_today`) and `UTC` (`due_soon` day 1) · the whole suite passes under
`TZ=Pacific/Kiritimati` · an unknown Graph zone yields `due: null` + `due_unresolved`, task
still listed, warned once · warm agenda ≤ 15 ms.

**Files:** `domain/datetime.rs`, `tools/read.rs` (`todo_agenda`),
`tests/{datetime_edges,agenda_buckets}.rs` and their fixtures.

---

## 1. The off-by-one this milestone exists to eliminate

Microsoft's own `Get todoTask` sample response:

> *`"dueDateTime": {"dateTime": "2020-08-25T04:00:00.0000000", "timeZone": "UTC"}`*

`04:00Z` is a date typed as "Aug 25" in a western-hemisphere mailbox, stored as an instant.
A real-world report: mailbox at UTC+2, UI due date **2023-04-15**, Graph returns
`"2023-04-14T22:00:00.0000000"` / `"UTC"`. **Verified by running the arithmetic:** that
string's date part is **2023-04-14 — the wrong day**; the same instant read in
`Europe/Prague` recovers **2023-04-15**.

The rule, decided here and pinned by tests: **a due date is the civil date of `dateTime`
interpreted in the zone named by `timeZone`, converted into the effective zone, with the time
of day discarded.** And **we cannot read the mailbox's own zone**: `/me/mailboxSettings`
needs `MailboxSettings.Read`, outside `Tasks.Read`/`Tasks.ReadWrite`, and asking for it would
break the single-scope claim. The anchor comes from the operator.

---

## 2. Configuration

Precedence: the per-call `timezone` argument (the three date-bearing read tools —
`todo_search_tasks`, `todo_get_task`, `todo_agenda` — and the two date-writing tools; **not**
`todo_lists` or `todo_account_status`, whose argument lists are closed in
[`m4-cache-read-tools.md`](m4-cache-read-tools.md) §4 and whose output carries no date; that call
only, echoed as `structuredContent.timezone`) → `TODO_MCP_TZ` → **`UTC` with
a loud startup WARN**. The OS `TZ` variable and `/etc/localtime` are **never** read and are
not reachable (§3). The fallback is the shipped surface — `README.md` documents `UTC` *(with
a warning)* and `.env.example` agrees; the earlier unpublished research's required-with-no-default and `TZ` →
`/etc/localtime` → hard-fail ladders are both **rejected**, the second doubly so, its rungs
being what the `TZ=Pacific/Kiritimati` run disproves. Acceptance is a **case-insensitive
scan of `chrono_tz::TZ_VARIANTS`** ([calendar-ics-mcp](https://github.com/hromadkom/calendar-ics-mcp)'s `is_valid_iana`), not a
bare `Tz::from_str`; a rejected value is a **startup refusal** — a wrong zone is worse than
no zone. `doctor` is the exception and **never requires it**: it is what a stuck user runs,
so a missing or invalid zone is a finding plus remediation and the date-interpretation
section is skipped.

The warning is verbatim, and it names where the user finds the answer:

> `WARN TODO_MCP_TZ is unset; using UTC. Microsoft To Do anchors due dates to your`
> `mailbox's time zone — with the wrong zone, "due today" is off by one day for part`
> `of every day. Set TODO_MCP_TZ to the IANA zone shown in Outlook → Settings →`
> `Language and time.`

A zone that is *valid* but disagrees with the real mailbox is undetectable — no `/me`, no
`mailboxSettings`, nothing to compare against. The one affordance, and the whole diagnostic story here:
**`doctor` prints one real task's raw `dueDateTime` beside the interpreted local date.**

> **The refusal must not echo the bad value; the tool error must.** M1's
> `tests/config_refusals.rs` asserts that **no** `validate()` refusal echoes a configured
> value ([`m1-auth.md`](m1-auth.md) §7). A zone name is not a secret, but an exception carved
> into a blanket rule costs more than it buys: name the variable, give an example. The
> **per-call** argument is the opposite case — the model's own input coming back — so
> `unknown timezone "<v>"; expected an IANA name like "Europe/Prague".` is an `isError` that
> quotes it, as it should.

---

## 3. The crate split, and why `chrono::Local` does not exist

| Crate | Version / features | Job |
|---|---|---|
| `chrono` | 0.4, `default-features = false`, **`["std", "now"]`** | instants, `NaiveDateTime`, `NaiveDate`, `Days`, formatting |
| `chrono-tz` | 0.10 | the IANA tzdb **compiled into the binary** |
| `windows-timezones` | 0.5, `["chrono-tz-0_10"]` + defaults | the CLDR Windows↔IANA table, for exactly two jobs |

The earlier unpublished research's table said `["clock", "std"]`. **The shipped `Cargo.toml` is `["std", "now"]`, and the
difference is the whole invariant:** `clock` pulls `iana-time-zone` and defines `chrono::Local`.
Without it the host timezone is **structurally unreachable (E0433)**, not merely lint-banned — gate 6
asserts `iana-time-zone` is absent from `Cargo.lock`, and `clippy.toml` still bans `chrono::Local` with
`allow-invalid = true` so re-adding `clock` is a lint error, not a silent correctness bug.
`Utc::now()` survives on `now` and is itself `disallowed-methods`-banned: wall-clock reads go through
the injected `Clock`, `logger.rs` being the single sanctioned caller with a local `#[allow]`.

`chrono-tz` compiling the tzdb into the binary is what makes **`FROM scratch` viable** — the image has
no `/usr/share/zoneinfo`, and a system-tzdb crate would silently degrade to UTC-only there. Measured,
for M8's footprint page only (**the crate choice is settled, not reopened here**): `windows-timezones`
alone **+16,528 bytes** over an empty binary, `chrono{clock,std}` + `chrono-tz` **+1,238,992**, a
bundled-tzdb `jiff` stack 841,760 bytes smaller — macOS aarch64, `opt-level="z"`, `lto`, `abort`.

Verified API surface (compile-and-run checked on rustc 1.97.1):

```rust
WindowsTimezone::try_from(chrono_tz::Tz) -> Result<WindowsTimezone, FromChronoTzError>
WindowsTimezone::name(self) -> &'static str      // "Central Europe Standard Time"
WindowsTimezone::tzdb_id(self) -> &'static str   // "Europe/Budapest"
impl From<WindowsTimezone> for chrono_tz::Tz     // gated on `chrono-tz-0_10`, NOT `chrono-tz`
impl FromStr for WindowsTimezone                 // gated on `std` — a DEFAULT feature here
```

Naming that gate `chrono-tz` compiles fine and then fails to find the `From` impl; and a
`default-features = false` tidy-up silently deletes the `FromStr` `parse_graph_tz` needs.

> **`domain/datetime.rs` is the only caller of `from_local_datetime` — a grep gate, not a
> convention.** The idiomatic call is `tz.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()`, which
> **panics** on the three dates in §5. One chokepoint means one place to get the three-arm
> match right; scattered day-boundary arithmetic means auditing every one of them forever.
> Add it to [`scripts/gates.sh`](../../scripts/gates.sh) as gate 10.

---

## 4. `resolve()` — one function, both paths

`parse_graph_tz` is the ladder for whatever string Graph put in
`dateTimeTimeZone.timeZone`. Verified behaviour of this exact function:

| Input | Output | | Input | Output |
|---|---|---|---|---|
| `"UTC"` | `UTC` | | `"Pacific Standard Time"` | `America/Los_Angeles` |
| `"Etc/UTC"` | `Etc/UTC` | | `"Eastern Standard Time"` | `America/New_York` |
| `"Europe/Berlin"` | `Europe/Berlin` | | `"GMT Standard Time"` | `Europe/London` |
| `"W. Europe Standard Time"` | `Europe/Berlin` | | `"tzone://Microsoft/Utc"` | `None` |
| `"Central Europe Standard Time"` | **`Europe/Budapest`** | | `"Customized Time Zone"` | `None` |

```rust
/// IANA first (covers "UTC"), else the Windows name via `w.tzdb_id()`, else None.
/// The `None` arm never falls back to UTC — a guessed zone is the §1 bug again.
pub fn parse_graph_tz(s: &str) -> Option<Tz>;

pub struct Resolved { pub instant: DateTime<Utc>, pub local: DateTime<Tz> }

/// Parses `dtz.date_time` ("%Y-%m-%dT%H:%M:%S%.f", then again without the fraction) as a
/// WALL CLOCK in `parse_graph_tz(&dtz.time_zone)` — unknown zone ⇒ TzError::UnknownZone,
/// NEVER UTC — then matches `src.from_local_datetime(&naive)` with no `_ =>` arm:
///   Single(dt)      → dt
///   Ambiguous(a, _) → a, the EARLIER instant. Same local date either way, so the choice
///                     is invisible to every predicate and only stabilises ordering.
///   None            → shift_forward_to_valid(src, naive)?   // the spring-forward gap, §5
/// and returns `local: instant.with_timezone(&tz)` — the field every predicate reads.
pub fn resolve(dtz: &DateTimeTimeZone, tz: Tz) -> Result<Resolved, TzError>;

/// 15-minute steps, up to 3 h, until a valid local time exists; the local DATE is
/// unchanged, and the local date is all the predicates depend on.
fn shift_forward_to_valid(src: Tz, naive: NaiveDateTime) -> Result<DateTime<Utc>, TzError>;
```

An unresolvable zone must **not** fail the whole tool. That task gets `due: null` plus `due_unresolved:
"<raw timeZone string>"`, still appears in listings, and lands in **no** bucket — warned **once per
process** (`warned_tzids: HashSet<String>`), since one bad zone is on hundreds of tasks.

> **Never round-trip IANA → Windows → IANA.** Run, not assumed: `Europe/Prague` →
> `"Central Europe Standard Time"` → **`Europe/Budapest`**. That is *correct* CLDR-001
> behaviour — the mapping is Windows-name to **territory default** and Prague is not it — but
> a round trip therefore substitutes a different IANA name. Both zones share offsets and DST
> rules, so every **instant** agrees and nothing observable breaks; what breaks is trust, when
> a result echoes `Europe/Budapest` at a Prague user. **The authoritative zone for every
> computation is the `Tz` the operator configured (or the per-call override), full stop**; the
> Windows name has two outbound jobs only, the `Prefer` header (§10) and the `timeZone` of a
> date write (§8). Any result echoing a zone echoes the **original Graph string** beside the
> resolved one; test 9 pins it.

---

## 5. `LocalResult::None` is real, and it is a crash without §4

In a handful of zones DST springs forward **at midnight**, so local midnight does not exist
on that date. Measured, both crates in one binary: `chrono` returns **`None`** from
`from_local_datetime` for `America/Santiago` 2026-09-06 00:00, `Asia/Beirut` 2026-03-29
00:00 and `America/Havana` 2026-03-08 00:00.

`with_ymd_and_hms(y, m, d, 0, 0, 0)` returns `MappedLocalTime::None` and the idiomatic
`.unwrap()` is a live crash for every user in Santiago, Beirut or Havana on those three days
a year. **`shift_forward_to_valid` was then run on all three and lands at 01:00 with the
local date unchanged** — the same answer the `jiff` comparison build's Temporal-"compatible"
disambiguation gave in that run (`2026-09-06T01:00:00-03:00`, `2026-03-29T01:00:00+03:00`,
`2026-03-08T01:00:00-04:00`). The *general* claim that the shift never moves the date is
reasoned, not swept — §12 says how to settle it.

Where midnight *does* exist on a transition day nothing special happens (`Europe/Prague`
2026-03-29T00:00 → `Single(00:00 CET)`; calendar arithmetic makes a 23-hour day irrelevant),
and the year boundary is the same story: `2026-12-31T23:00:00Z` in `Europe/Prague` is
**2027-01-01**, `NaiveDate` ordering calendar-correct across it. **No special case at all.**

---

## 6. The bucket predicates, defined exactly

Let `tz` be the effective zone, `today = clock.now().with_timezone(&tz).date_naive()`, and
per task
`due_local: Option<NaiveDate> = resolve(dueDateTime, tz).ok().map(|r| r.local.date_naive())`.

```
overdue(t)      ⇔ status ≠ "completed" ∧ due_local = Some(d) ∧ d <  today
due_today(t)    ⇔ status ≠ "completed" ∧ due_local = Some(d) ∧ d == today
due_soon(t, h)  ⇔ status ≠ "completed" ∧ due_local = Some(d) ∧ today < d ≤ today.checked_add_days(Days::new(h))
no_due_date(t)  ⇔ status ≠ "completed" ∧ due_local = None    ∧ dueDateTime ABSENT
```

Boundary sides, because they are what a refactor flips: `overdue` is **strictly** `<`, so a
task due today is never overdue; `due_today` is `==`; `due_soon` is **exclusive** at `today`,
**inclusive** at `today + h`. The four partition rather than overlap; completed tasks are in
none of them (`recently_completed`, §7, selects on `completedDateTime`). **These are
`NaiveDate` comparisons, never instant comparisons:** To Do has no due *times*, only due
dates, so any instant comparison re-introduces the §1 off-by-one. `today + h` is
`checked_add_days(Days::new(h))` — calendar arithmetic, never `+ 86400 * h`, verified to work
without `clock`. On the wire `TaskSummary.due` is that same local date as `YYYY-MM-DD`, never
an instant, never `Z`-suffixed; `todo_search_tasks`'s `due` keywords reuse this
`due_local`/`today` pair rather than deriving their own
([`m4-cache-read-tools.md`](m4-cache-read-tools.md) §4).

> **An unresolved zone is not "no due date".** `no_due_date` requires `dueDateTime` to be
> **absent from the payload**. A task whose `timeZone` we could not map has a deadline we
> cannot place — it carries `due_unresolved` and belongs to no bucket. Filing it under
> `no_due_date` tells the model "this has no deadline": a confident lie about a task that may
> be a week overdue.

---

## 7. `todo_agenda` — why one call, and what it costs

Annotations: `readOnlyHint: true`, `destructiveHint: false`, `idempotentHint: true`,
**`openWorldHint: true`** (the MCP default is `true`; the superseded plan had it backwards) —
untrusted per spec, so read-only semantics are enforced server-side regardless. Input:
`timezone`, `horizon_days` (1–30, default 7), `include_completed_since_days` (0–30, default
1), `max_per_section` (1–50, default 15), `lists` (≤20 names; omit for all). Output is
`{account_id, timezone, today, complete, coverage, truncation, sections}`, each section
`{count, truncated, tasks[]}` with `count` the **true pre-truncation** count:

| Section | Contents |
|---|---|
| `overdue` | §6 `overdue`, sorted by `due` ascending |
| `due_today` | §6 `due_today` |
| `due_soon` | §6 `due_soon(horizon_days)` |
| `no_due_date` | §6 `no_due_date`, **hard-capped** at `max_per_section`, sorted `created_desc` — in a real mailbox it is an unbounded backlog that would swamp the five sections a model actually needs |
| `flagged_emails` | open tasks in the `flaggedEmails` well-known list |
| `recently_completed` | `completedDateTime` within `include_completed_since_days` local days |

**Why one call and not five.** The alternative is `todo_lists` plus N searches, and every
round trip is a model turn — the latency a user feels is turns × (model + network). One call
also means one consistent `today`: five straddling local midnight can double-bucket a task.

**The ≤ 15 ms budget is the warm path, and it is what makes "one call" honest.** A warm agenda issues
**zero** Graph requests: it reads the M4 TTL cache under a read lock, evaluates §6 over the cached
tasks, and renders — nothing in the warm path syncs. Cold, it goes through M4's bounded first-call
sync and may return `complete: false` with `coverage` populated, which never sets `isError`
([`m4-cache-read-tools.md`](m4-cache-read-tools.md), written by M4). `todo_agenda` has **no cursor**,
and M4's per-tool response-cap ladder (§7 there) has no column for it: **this is that column.** Step 1
drops every `body_preview`; step 2 halves `max_per_section`, setting `sections.*.truncated: true` with
`count` still the true total, down to a floor of 1 per section; still over ⇒ `isError`. **Never drop a
whole section** — an absent `overdue` reads as "nothing is overdue".

---

## 8. Outbound date writes — the contract M6 consumes

| Argument | Exact JSON |
|---|---|
| `due_date: "2026-08-25"` | `{"dueDateTime":{"dateTime":"2026-08-25T00:00:00.0000000","timeZone":"<W>"}}` |
| `start_date: "2026-08-20"` | `{"startDateTime":{"dateTime":"2026-08-20T00:00:00.0000000","timeZone":"<W>"}}` |
| `reminder_at: "2026-08-25T09:00"` | `{"reminderDateTime":{"dateTime":"2026-08-25T09:00:00.0000000","timeZone":"<W>"},"isReminderOn":true}` |

`<W>` is `prefer_tz_value(tz)` when it resolves — the documented-safe Windows alphabet — else
`tz.name()` **only if** it is in the IANA list `dateTimeTimeZone` enumerates:

> *"In general, the **timeZone** property can be set to any of the time zones currently
> supported by Windows, as well as the other time zones supported by the calendar API."*

followed by an explicit list of additional IANA names (`Europe/Berlin`, `America/New_York`,
`Asia/Kolkata`, …). **UNVERIFIED:** that list was seen but never captured, so membership
cannot be tested — capture it into `../graph-probe.md` (M2's probe suite writes that file,
[`m2-mcp-http.md`](m2-mcp-http.md) §10) or treat this branch as unavailable. If `tz` is in
neither set, **refuse the write**:

> `Cannot write a date in time zone "<tz>": Microsoft Graph accepts only Windows`
> `time-zone names or a specific list of IANA names, and this zone is in neither. Set`
> `TODO_MCP_TZ (or the timezone argument) to a nearby supported zone such as`
> `"<suggestion>".`

`<suggestion>` is **not** a free-text guess. In order: CLDR's older spelling of the same zone
(`Europe/Kyiv` → `Europe/Kiev`, `Asia/Kolkata` → `Asia/Calcutta` — those *are* the primaries
`try_from` knows); else the first `chrono_tz::TZ_VARIANTS` entry for which `prefer_tz_value`
resolves and whose UTC offset at `now` matches; else `timezone: "UTC"`, saying in the same
breath that the date is then **UTC-anchored**. Pin the choice with a test. Writing UTC
midnight *silently* is the one thing that must never happen.

> **`T00:00:00` is sometimes a wall clock that does not exist.** On the three §5 dates a
> `due_date` write hands Graph a local midnight that never happens in that zone. Whatever
> instant Graph picks, the read path saves us: §4's `None` arm shifts forward and recovers
> the same local date, so the round trip is stable even though the write was not well
> defined. **UNVERIFIED** how Graph disambiguates — probe J's re-`GET` shows it, and a
> Santiago fixture is the cheapest place to look.

---

## 9. `ALIAS_FALLBACK`, and why probe J blocks M6

Not every IANA zone maps. Run: `Asia/Kolkata` → `Err(FromChronoTzError)`, `Europe/Kyiv` →
`Err(FromChronoTzError)` — CLDR's primaries are the older `Asia/Calcutta`, `Europe/Kiev`.
So `prefer_tz_value(tz: Tz) -> Option<&'static str>` is `try_from(tz).name()`, else a
hand-written `ALIAS_FALLBACK` entry, else `None` — **outbound only**, and never fed back
through `parse_graph_tz` (§4).

| IANA zone | Fallback Windows name | `try_from(Tz)` | In Microsoft's IANA list? |
|---|---|---|---|
| `Asia/Kolkata` | `"India Standard Time"` | `Err`, **verified** | **yes** — §8's branch already saves it |
| `Europe/Kyiv` | `"FLE Standard Time"` | `Err`, **verified** | **no** — the hard case |
| `Asia/Ho_Chi_Minh` | `"SE Asia Standard Time"` | `Err`, **verified** | unknown (list uncaptured) |
| `America/Nuuk` | `"Greenland Standard Time"` | `Err`, **verified** | unknown (list uncaptured) |
| `Europe/Uzhgorod` | `"FLE Standard Time"` | **UNVERIFIED** | unknown |
| `Europe/Zaporozhye` | `"FLE Standard Time"` | **UNVERIFIED** | unknown |

The four `Err` results were reproduced in one run; the two Ukrainian aliases were not re-run.
Every name in column 2 is hand-written and **UNVERIFIED against Graph** — that is what probe
J settles: `PATCH` a date with `timeZone: "India Standard Time"`, then re-`GET`.

> **Probe J is an M6 blocker, not a nice-to-have.** Without `ALIAS_FALLBACK` a `Europe/Kyiv` user
> **cannot write a due date at all**: `prefer_tz_value` returns `None`, Microsoft's enumerated IANA
> list does not contain `Europe/Kyiv`, and §8 therefore refuses. It is the sharpest case because it is
> the one the IANA branch cannot rescue — `Asia/Kolkata` *is* in that list — and it is not exotic: it
> is the current spelling of a zone with tens of millions of people in it. Today, with the list still
> uncaptured, **every** row above refuses.
>
> The fallback makes the write *possible*; probe J says whether it *works*. If Graph rejects those
> strings, §8's refusal is the only correct behaviour and its `<suggestion>` must name a substitute
> that really maps — for Kyiv that is `Europe/Kiev`, CLDR's own primary, **UNVERIFIED** end to end.
> **Run probe J before writing `tools/write.rs`** ([`m6-write-tools.md`](m6-write-tools.md), written
> by M6). On the read path a `None` is harmless: `Prefer` simply omits its timezone half.

---

## 10. The `Prefer` header — an optimisation, never a dependency

Every read carries `Prefer: outlook.timezone="Central Europe Standard Time",
odata.maxpagesize=100`. Per RFC 7240 an applied preference comes back as
`Preference-Applied: outlook.timezone="…"`; the first response of the process sets
`timezone_mode ∈ {server_side, client_side, unknown}` once, logs it, and
`todo_account_status` reports it.

**It is undocumented for To Do.** Verified by direct contrast of the v1.0 reference docs:
`event-get.md`'s request-header table lists `Prefer: outlook.timezone`, while `todotask-get.md`,
`todotasklist-list-tasks.md`, `todotasklist-post-tasks.md`, `todotask-update.md`,
`todotask-delete.md`, `todo-list-lists.md` and `todo-post-lists.md` list **only `Authorization`**
(plus `Content-Type` on writes).

That is survivable because **both paths run the same `resolve()`**: honoured means `src == tz` and the
conversion is a no-op; ignored means `src == UTC` and §4 does the real work. Whether Graph returns 400,
ignores it, or honours it — and whether the answer survives inside a `$batch` sub-request, where the
header nests in the sub-request's own `headers` object — is **UNVERIFIED** until probes **K/L** run
([`m2-mcp-http.md`](m2-mcp-http.md) §10).

---

## 11. Tests

| # | Test | Assertion |
|---|---|---|
| 1 | non-existent local midnight | `America/Santiago` 2026-09-06, `Asia/Beirut` 2026-03-29, `America/Havana` 2026-03-08 → `resolve` is `Ok` at 01:00, local **date** unchanged, no panic |
| 2 | fall-back ambiguous hour | 2026-10-25T02:30 `Europe/Prague` → the earlier instant; same local date |
| 3 | spring-forward day that *does* have midnight | task due 2026-03-29, `tz=Europe/Prague`, clock 2026-03-29T05:00Z ⇒ `due_today` |
| 4 | half-hour-offset transition | `Australia/Lord_Howe` 2026-10-04T02:15 must resolve, not error |
| 5 | **year boundary** | one task due 2027-01-01 local, clock 2026-12-31T23:30Z: `Europe/Prague` ⇒ `due_today`; `UTC` ⇒ `due_soon` day 1. **Both must pass in one run.** |
| 6 | **two-timezone differential** | one fixture, `Pacific/Kiritimati` (UTC+14) vs `America/Los_Angeles` — assert bucket *membership* differs, not merely the counts |
| 7 | unknown Graph zone | `"tzone://Microsoft/Custom"` → `due: null`, `due_unresolved` set, task listed, in **no** bucket; a second task with the same zone emits **no** second warning |
| 8 | host-TZ independence | container `TZ=America/Denver` + `TODO_MCP_TZ=Europe/Prague` ⇒ identical results; and the whole suite re-run under `TZ=Pacific/Kiritimati` |
| 9 | round-trip lossiness is pinned | `prefer_tz_value(Europe/Prague) == "Central Europe Standard Time"` **and** `Tz::from(that) == Europe/Budapest` — so nobody "fixes" §4 |
| 10 | warm agenda | zero requests against the fixture Graph counter; six sections present with `complete: true` |
| 11 | the write path refuses **usefully** | `prefer_tz_value(Europe/Kyiv) == Some("FLE Standard Time")` via `ALIAS_FALLBACK`; a zone in neither set yields §8's refusal, and the `<suggestion>` it names is itself writable |

Case 8's suite run is the hermetic-gate composite re-running the **already compiled** test binaries
under `TZ=Pacific/Kiritimati` — a second pass that rebuilds nothing. Every test drives the injected
`Clock`: one reading the wall clock turns cases 3, 5 and 6 into time bombs that fail one day a year.

---

## 12. Settle during M5

- **Probe J** — the six `ALIAS_FALLBACK` strings are hand-written and unverified against
  Graph. Blocks M6 (§9). Run it with M2's probe suite; record the answer
  (redacted per [`m8-docs-release.md`](m8-docs-release.md) §6).
- **Probes K/L** — `Prefer: outlook.timezone` on a `/todo` endpoint: 400, ignored, or
  honoured; and whether it survives inside `$batch` (§10).
- **The enumerated IANA list** in the `dateTimeTimeZone` docs was seen but never captured, so
  §8's IANA branch cannot be tested. Capture it, or ship the Windows alphabet alone.
- **`shift_forward_to_valid` beyond the three measured zones.** It was run on all three and lands at
  01:00 with the date unchanged (§5); *"the local date never moves"* as a general claim is reasoned,
  not swept. Settle it with a sweep over every `chrono_tz::TZ_VARIANTS` entry at each transition —
  which also says whether the 3-hour give-up bound is generous.
- **≤ 15 ms is a target, not a measurement.** Measure the warm path against a real mailbox's task
  count and hand the number to M8's footprint page.
- **`doctor` and `TODO_MCP_TZ`** — the earlier unpublished research marks the variable required for `doctor`
  *and* says `doctor` never refuses. This spec rules never required (§2); check M1 agrees.

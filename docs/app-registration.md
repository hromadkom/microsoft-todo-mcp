# Creating your Entra app registration

This server ships **no client ID**. You register a small app in your own Microsoft
account or tenant, and this server signs in as that app.

That is not laziness. Under Microsoft's default (managed) consent policy, end users
on a work or school tenant cannot consent to `Tasks.ReadWrite` for any app,
publisher-verified or not; see [Work or school accounts](#work-or-school-accounts).
An app registered by *you* needs no admin at all for a personal Microsoft account,
and gives your tenant admin something concrete to approve if you use a work account.

**Time: about 10 minutes.** You need it once.

> **Portal UIs move.** The click paths below were accurate when written; the
> **manifest values** in step 9 are the ground truth. If a screen doesn't look like
> this, check the manifest instead of hunting for the button.

---

## Before you start

You need to be able to register applications at all. Many tenants set
`usersCanRegisterApplications = false`.

- **Personal Microsoft account** (outlook.com, hotmail.com, live.com) — this is the
  smoothest path and the one this project targets (not yet verified live). No admin,
  no consent policy, no Conditional Access.
- **Work or school account** — you can usually register an app, but you will very
  likely need an administrator to approve the permission. See
  [Work or school accounts](#work-or-school-accounts) before investing time.

> **Unverified:** whether signing in to `entra.microsoft.com` with a bare personal
> Microsoft account still provisions a usable directory, or whether you must first
> create a free Azure account. Microsoft's own prerequisite reads *"A Microsoft
> Entra ID tenant. If you don't have a tenant, create a free Azure account."*
> **Try it first** — if the portal refuses to let you register, create the free
> account and retry. It does not require a paid subscription.

---

## Register the app

1. Sign in at **<https://entra.microsoft.com>**.

2. If you belong to more than one tenant, use the **Settings** gear in the top bar
   to switch to the one you want.

3. Go to **Entra ID → App registrations → New registration**.
   *(Older tenants show* **Identity → Applications → App registrations** *— same
   place.)*

4. **Name:** `todo-mcp` (only you ever see this).

5. **Supported account types:** choose the option covering **any Entra ID tenant
   *and* personal Microsoft accounts**. Depending on the portal version this reads
   either *"Any Entra ID Tenant + Personal Microsoft accounts"* or *"Accounts in
   any organizational directory (Any Microsoft Entra ID tenant - Multitenant) and
   personal Microsoft accounts"*.

   Getting this wrong produces **AADSTS50194** at login.

6. **Redirect URI: leave it empty. Do not add a platform.**

   Device code flow does not use one. Microsoft, verbatim: *"To distinguish device
   code flow, integrated Windows authentication, and a username and a password
   from a confidential client application using a client credential flow used in
   daemon applications, none of which requires a redirect URI, configure it as a
   public client application."*

7. Click **Register**. On the **Overview** page copy **Application (client) ID** —
   that is your `TODO_MCP_CLIENT_ID`.

8. **The one toggle everybody misses.** Go to **Manage → Authentication →
   Advanced settings → Allow public client flows → Yes → Save.**

   This is off by default, and it lives on a different blade from where you just
   registered. Skipping it is the single most common setup failure and produces
   **AADSTS7000218** — an error whose text asks for a client secret, which this
   server will never send.

9. **Verify in the manifest** (**Manage → Manifest**). Portal wording changes;
   these values do not:

   | Key | Required value |
   |---|---|
   | `signInAudience` | `AzureADandPersonalMicrosoftAccount` |
   | `allowPublicClient` **or** `isFallbackPublicClient` | `true` |

   > **Why two names.** The portal has two manifest schemas. The *Azure AD Graph*
   > format calls it **`allowPublicClient`**; the *Microsoft Graph* format calls it
   > **`isFallbackPublicClient`**. You will see exactly one of them depending on
   > which format the editor is showing — they are the same setting. Microsoft, on
   > `allowPublicClient`: *"If this value is set to true the fallback application
   > type is set as public client, such as an installed app running on a mobile
   > device. **The default value is false**"* — which is why step 8 is necessary at
   > all.

10. **Manage → API permissions → Add a permission → Microsoft Graph → Delegated
    permissions**, search `Tasks`, tick **`Tasks.ReadWrite`**, then **Add
    permissions**.

    | Permission | Display name | Admin consent required |
    |---|---|---|
    | `Tasks.ReadWrite` | Read and write user tasks | No |
    | `Tasks.Read` (read-only server) | Read user tasks | No |

    `TODO_MCP_SCOPE=Tasks.Read` is a ceiling: it is enough to keep the five write
    tools out of `tools/list`, even if this registration also carries
    `Tasks.ReadWrite`. The stored credential is read-only only if the registration
    grants just `Tasks.Read`; when Microsoft grants `Tasks.ReadWrite` anyway,
    `login`, `serve` and `doctor` warn that the refresh token can still write your
    tasks.

    > Permission GUIDs are deliberately not listed here — you tick a checkbox by
    > name and never type an ID. If you need one (for a scripted
    > `Grant admin consent`, say), read it off **API permissions** in the portal
    > after adding it, or from
    > <https://learn.microsoft.com/graph/permissions-reference>. Do not copy one
    > out of a blog post; several widely-circulated values are wrong.

    - **Do not** add `Tasks.Read.All` or `Tasks.ReadWrite.All`, and do not open
      **Application permissions** at all. Those are org-wide, need admin consent,
      and cannot write To Do anyway (see [What this app can do](#what-this-app-can-and-cannot-do)).
      If a `.All` permission is granted anyway, `login` refuses and saves nothing,
      `serve` refuses to start (or, if already running, to refresh), and `doctor`
      reports it. Once consent has been given, removing the permission from this
      list is not enough: also [revoke the consent](#revoking-consent), then run
      `login` again.
    - **Do not** add `Tasks.ReadWrite.Shared`. `Tasks.ReadWrite` already covers
      lists shared with you, and the `.Shared` scopes are Outlook-Tasks holdovers
      that appear in no To Do endpoint's permission table.
    - `User.Read` is usually added automatically. **Remove it**: this server never
      calls the profile endpoint `GET /me` and never asks for your identity
      (`openid`, `profile` and `User.Read` are not requested). If it stays granted,
      `login`, `serve` and `doctor` print a warning naming it, and nothing else
      changes. If you already consented to it, [revoke the consent](#revoking-consent)
      after removing it, then run `login` again.
    - You do **not** need to add `offline_access` here; the v2.0 endpoint consents
      dynamically from the scope in the request.

11. **Do not create a client secret.** This is a public client. If one already
    exists it is simply unused, and setting `TODO_MCP_CLIENT_SECRET` makes this
    server refuse to start rather than quietly ignore it.

---

## Use it

Put the ID into `.env` in your checkout of this repository, on the existing
`TODO_MCP_CLIENT_ID=` line. The README's Quick start creates `.env` with
`cp .env.example .env`, which ships that line empty and overwrites any `.env`
already there — so copy first, then **set** the line rather than appending a second
one. Then sign in:

```bash
docker compose run --rm todo-mcp login
```

`login` prints a URL and a code. Open the URL, enter the code, approve the consent
screen. **Use the URL the server prints, verbatim** — personal accounts and work
accounts are sent to different endpoints, and the values change over time. `login`
needs no TTY, and Ctrl-C stops the wait.

You only do this once. The refresh token is stored in the compose volume
`microsoft-todo-mcp_todo-mcp-state` at `/data/token.json`, mode `0600`. Without
Compose, run `todo-mcp login` with the same variables in its environment (see the
README's host-toolchain section).

---

## Work or school accounts

Read this before you spend time on it.

Microsoft's managed user-consent policy — the setting labelled *"Let Microsoft
manage your consent settings"*, which Microsoft states **"is also the default for a
new tenant"** — excludes these from what an end user may consent to, verbatim:

> End users can consent for any user consentable delegated permissions EXCEPT:
> For Microsoft Graph: … `Tasks.Read`, `Tasks.Read.Shared`, `Tasks.ReadWrite`,
> `Tasks.ReadWrite.Shared`, `People.Read`.

So in a default-configured tenant an ordinary user **cannot** self-consent to
`Tasks.ReadWrite` — not even for an app they registered themselves, and publisher
verification does not unblock it. This is the single biggest reason work/school
accounts are harder than personal ones here.

You will find out when `login` fails with **AADSTS90094**.

**If you are not an administrator:** ask a Global Administrator or Cloud
Application Administrator to open **entra.microsoft.com → App registrations →
*your app* → API permissions → Grant admin consent for \<tenant\>**. Until they do,
this account cannot be used.

**If your tenant blocks device code flow** via a Conditional Access
authentication-flows policy, `login` fails with **AADSTS530036**. v1 ships no other
sign-in flow, so that account cannot be used at all. If the policy arrives after you
signed in, the next token refresh (in `serve` or `doctor`) fails with the same code
and the server deletes `token.json`, because Microsoft states such a token will never
be usable.

A personal Microsoft account has none of these gates. If work/school turns out to
be blocked, that is the fallback.

> Note that `Tasks.ReadWrite`'s own "Admin consent required: **No**" flag is a
> *different question* from whether your tenant's consent policy permits it. The
> flag is necessary, not sufficient — the consent policy above is what actually
> decides. Your admin may also have replaced the managed policy with a custom one,
> in either direction.

---

## Revoking consent

Removing a permission from the app registration does not withdraw consent that was
already given: Microsoft may keep granting a scope you consented to earlier. To take
it back:

- **Personal Microsoft account:** open <https://account.microsoft.com/privacy/app-access>
  and remove the app's access.
- **Work or school account:** an administrator revokes it under **Enterprise
  applications → *your app* → Permissions**.

Then run `login` again. `logout` is not a substitute: it deletes `token.json` but
revokes nothing at Microsoft.

> These click paths have not yet been verified against a live portal. What *has*
> been observed (2026-09-22, single-tenant work account, `docs/graph-probe.md`):
> the granted scope string was `profile openid email
> https://graph.microsoft.com/Tasks.ReadWrite https://graph.microsoft.com/User.Read`
> — the portal's default `User.Read` plus the OpenID trio, no `.All` — so the
> "Microsoft also granted …" warning is the normal first-run outcome, and revoking
> is the only way to make it go away.

---

## Troubleshooting

`todo-mcp doctor` reports token state, granted scopes and Graph reachability, and
names the fix for anything it detects. If you want to look a code up yourself, the
authoritative lookup is `https://login.microsoftonline.com/error?code=<number>`.

| Code | What it actually means | Fix |
|---|---|---|
| **7000218** | The message asks for a `client_secret`, but the real cause is that your app is not marked as a public client. **The most common failure.** | Step 8 — Authentication → Allow public client flows → **Yes** |
| **50194** | App is single-tenant but the server used the `/common` endpoint | Step 5 — change Supported account types, or set `TODO_MCP_TENANT` to your tenant GUID or domain |
| **90094** | Your tenant requires an admin to approve this permission | [Work or school accounts](#work-or-school-accounts) |
| **65001** | Nobody has consented yet | Run `login` and accept the consent screen |
| **530036** | Conditional Access blocks device code flow; the token can never work. On a refresh (`serve`, `doctor`) `token.json` is deleted | Ask your admin to exempt the app, or use a personal account |
| **700016** | The client ID was not found in this tenant | Re-check **Application (client) ID** on the Overview page and `TODO_MCP_TENANT` |
| **50105** | The enterprise app has *Assignment required = Yes* | Admin assigns you under Enterprise applications → *your app* → Users and groups |
| **7000112** | The application is disabled | Enterprise applications → *your app* → Properties → **Enabled for users to sign in** → Yes |
| **70011** | Microsoft rejected the requested scope. This server only ever requests `Tasks.ReadWrite` or `Tasks.Read` plus `offline_access`, and refuses any other `TODO_MCP_SCOPE` before contacting Microsoft, so it is not a typo. The cause has not been verified against a live tenant | Check Supported account types (step 5) and API permissions (step 10), then run `login` again; if it persists, report it as described below |
| **70018** | The code typed in the browser was wrong | Run `login` again and type the code exactly as printed |
| **70019 / 70020** | The ~15-minute sign-in window closed | Run `login` again and finish in the browser |
| **65004** | You declined the consent screen | Run `login` again and accept |
| **70008 / 700082** | The refresh token expired or was revoked. Seen on a refresh (`serve`, `doctor`), where `token.json` is deleted because it can never be redeemed | Run `login` again |
| **9002313** | Microsoft rejected the request as malformed — a client bug or a transient fault, not a bad token. `token.json` is left untouched | Retry; if it persists, report it as described below |
| **7000215 / 7000222** | Microsoft expected a valid client secret. This server never sends one (`TODO_MCP_CLIENT_SECRET` is refused at startup), so the registration is being treated as a confidential client | Step 8 — Allow public client flows → **Yes**; an existing secret is unused and can be removed |
| **900023** | `TODO_MCP_TENANT` is not a valid tenant identifier | Use a tenant GUID, a verified domain, or `common` / `organizations` / `consumers` |
| **500011** | Microsoft Graph's service principal was not found in the tenant — unusual | An administrator must provision it, or use a personal account |
| **A `.All` permission was granted** (no AADSTS code) | Microsoft granted an organisation-wide `.All` permission. `login` refuses and saves nothing, `serve` will not start (or stops refreshing), and `doctor` reports it | Step 10 — remove the `.All` permission, [revoke the consent](#revoking-consent), then run `login` again |
| **Warning: Microsoft also granted …** (no AADSTS code) | Microsoft granted a scope this server never uses — typically the portal's default `User.Read`. The server runs normally, but the stored refresh token can obtain tokens carrying that scope | Step 10 — remove the permission, [revoke the consent](#revoking-consent), then run `login` again; or accept the warning |
| **Warning: Microsoft granted Tasks.ReadWrite; this server exposes only read tools** (no AADSTS code) | `TODO_MCP_SCOPE=Tasks.Read`, but the app registration also carries `Tasks.ReadWrite`, so Microsoft granted it. Only the tool list is read-only: the refresh token in `token.json` can write your tasks. When the previous warning applies too, both appear in one line | For a read-only credential: step 10 — remove `Tasks.ReadWrite`, [revoke the consent](#revoking-consent), then run `login` again; or accept the warning |

Every sign-in error from Microsoft is printed with Microsoft's own text and, when
Microsoft provides them, a **Trace ID** and **Correlation ID**. Give both to
Microsoft support, or quote them with the AADSTS code in a
[GitHub issue](https://github.com/hromadkom/microsoft-todo-mcp/issues).
**Redact your client ID, tenant ID and account name first**, and never paste
`token.json`, `bearer.token`, the output of `token`, or a device code. `doctor` output
also names your lists and can name a task title, so replace those before pasting it. If the
problem looks security-relevant, report it privately as [SECURITY.md](../SECURITY.md)
describes, not in a public issue.

---

## What this app can and cannot do

**Can:** read and write the To Do lists and tasks of the single account that signed
in.

**Cannot:**

- Read anyone else's tasks. The `/users/{id}/…` path shape exists nowhere in the
  codebase, and a build gate enforces that. Apart from the `$batch` endpoint itself,
  every Graph path is under `/me/todo/…`, and so is every `$batch` sub-request.
- Read your mail, files, calendar, contacts or profile. Those scopes are never
  requested. The server refuses to start if configured with any scope other than
  `Tasks.Read` or `Tasks.ReadWrite`, and refuses any token in which Microsoft grants
  a `.All` permission.
- Learn your name or email. It never calls the profile endpoint `GET /me` and never
  asks for your identity: `openid`, `profile` and `User.Read` are not requested, and
  every Graph request is under `/me/todo/`, directly or inside `$batch`.
- Act without you. There is no daemon identity and no client secret.

Worth knowing when you compare this to a general-purpose Microsoft 365 MCP server:
the org-wide alternative isn't merely broader, it **cannot do this job**. App-only
`Tasks.ReadWrite.All` is documented `"Not supported."` for every Microsoft To Do
write operation — only reads and the two delta functions carry an application
permission. Anything that writes your tasks must use a delegated token like this
one. The only question is what *else* that token can reach.

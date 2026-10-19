# Security Policy

## Reporting a vulnerability

Please do **not** open a public issue for security problems.

Report them privately through GitHub:
**Security → Advisories → Report a vulnerability** on this repository.

Include what you found, the affected version or commit, and the steps to
reproduce it. Once a fix is
released, it is recorded in this file and in [CHANGELOG.md](CHANGELOG.md).

## Supported versions

Only the latest release on `main` receives security fixes.

## Security model

This server runs in one of two modes, and the trust boundary differs.

- **Local (stdio).** One user runs the server for themselves. That user has
  full access to every Google Ads account their credentials can reach.
  Nothing below about tenants applies.
- **Remote (HTTP).** One process serves several *principals*, each with a
  bearer token, a role (`analyst`, `editor`, `admin`) and a list of allowed
  Google Ads customer IDs. Principals are grouped into *tenants*, and a
  customer ID can belong to only one tenant. The security goal is that no
  principal can read or change anything outside its own customer IDs, and
  only an `admin` can apply a change.

TLS is expected to be terminated by a reverse proxy in front of the server
(see [docs/remote-deployment.md](docs/remote-deployment.md)).

## Fixed vulnerabilities

### 1. Cross-tenant disclosure of pending change plans (Medium)

**Affects:** remote (HTTP) mode, versions up to and including 0.13.0.

**Problem.** Write tools do not change anything directly. They store a
*plan*, and `confirm_and_apply` executes it later. All principals in the
process shared one plan store, keyed only by a `plan_id`. When
`confirm_and_apply` was called with `dry_run=true` (the default), it
returned the full plan (customer ID, entity, and every proposed change)
*before* checking whether the caller was allowed to see that customer.

As a result, an `editor` in tenant B who obtained or guessed a `plan_id`
from tenant A could read tenant A's pending changes. Plan IDs were only
8 hex characters (about 4.3 billion values), and the deployment guide tells
editors to share them with an admin, so they could be both leaked and
enumerated. Applying another tenant's plan was **not** possible, because
the scope check still ran before any real change.

**Fix.**
- The customer-scope check now runs immediately after the plan is looked
  up, before anything is returned, dry runs included.
- A plan outside the caller's scope is reported as "not found", exactly
  like a plan that does not exist, so the response cannot be used to
  probe for valid IDs.
- Plan IDs are now full random UUIDs (122 bits) and cannot be guessed.

Code: `src/tools/confirm.rs`, `src/safety/preview.rs`.

### 2. Unbounded growth of the plan store (Low)

**Affects:** all modes, versions up to and including 0.13.0.

**Problem.** Plans that were drafted but never applied stayed in memory
until the process restarted. A careless or hostile principal with draft
rights could keep creating plans and grow memory without limit.

**Fix.**
- Pending plans expire 24 hours after they are created.
- Each Google Ads customer can have at most 500 pending plans. Past that,
  the oldest plan *for the same customer* is dropped. Because a customer
  belongs to only one tenant, one tenant cannot push out another tenant's
  plans.

Code: `src/safety/preview.rs`.

### 3. Google credentials sent to a non-Google API address (Low)

**Affects:** all modes, versions up to and including 0.13.0.

**Problem.** The `GOOGLE_ADS_API_BASE_URL` environment variable exists so
tests can point the client at a mock server. If it was set to any other
address while a credentials file was present, the server sent the real
Google OAuth access token and developer token to that address.

In addition, the check that decides whether an address is Google's
accepted anything *starting with* `https://googleads.googleapis.com`, so
a look-alike such as `https://googleads.googleapis.com.evil.example` was
treated as Google.

Only someone who controls the server's environment can set this variable,
so exploiting it requires a misconfiguration or another foothold.

**Fix.**
- When the API address is not Google's and a credentials file exists, the
  server refuses the request instead of sending credentials. Operators who
  really do need a custom endpoint must opt in with
  `GOOGLE_ADS_ALLOW_CUSTOM_BASE_URL=true`.
- An address only counts as Google's if `https://googleads.googleapis.com`
  is followed by `/` or nothing else.

Code: `src/client.rs`.

### 4. Forged sign-in callback in the token setup script (Low)

**Affects:** `scripts/generate_token.sh`, all versions up to and including
0.13.0.

**Problem.** The script signs you in to Google and listens on
`http://localhost:8085` for the redirect that carries the authorization
code. It did not use the OAuth `state` parameter, so any request that
reached that port first (for example, from a malicious web page open in
the same browser) could hand the script an authorization code for a
*different* Google account. The script would then save that account's
refresh token. The error text from the callback was also written into the
response page without HTML escaping.

**Fix.**
- The script generates a random `state` value, adds it to the sign-in URL,
  and ignores any callback that does not return the same value. It keeps
  waiting for the genuine redirect until the 2-minute timeout.
- Error text shown on the callback page is HTML-escaped.

Code: `scripts/generate_token.sh`.

## Upgrade notes

After updating to a version containing these fixes:

- A `plan_id` that belongs to another tenant now returns a "not found"
  error rather than "access denied".
- Pending plans older than 24 hours are discarded. Draft again if needed.
- If you intentionally route API calls through a non-Google endpoint with
  real credentials, set `GOOGLE_ADS_ALLOW_CUSTOM_BASE_URL=true`.

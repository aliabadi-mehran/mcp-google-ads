# Team Remote MCP deployment

This branch adds a multi-user Streamable HTTP transport intended to run behind
an HTTPS reverse proxy or ingress. The process refuses a remote configuration
whose public URL is not `https://`.

## Security model

Each principal has a dedicated MCP URL, bearer credential, service instance,
and session manager:

```text
HTTPS client
  -> TLS reverse proxy / ingress
  -> Bearer authentication
  -> /mcp/{principal-id}
  -> isolated MCP session manager
  -> RBAC + tenant/customer scope
  -> draft / server-side approval guard
  -> Google Ads API
  -> actor-attributed JSONL audit log
```

The path id is not a secret. Authorization depends only on a bearer token of at
least 32 characters. Store the token in an environment variable, or store only
its lowercase SHA-256 digest in the configuration. Never commit the token,
Google OAuth files, developer token, `.env`, or a populated `remote.json`.

### Roles

| Role | Read | Draft / dry-run | Apply real mutation |
|---|---:|---:|---:|
| Analyst | yes | no | no |
| Editor | yes | yes | no |
| Admin | yes | yes | yes |

An Editor can prepare a plan and share its `plan_id`. Applying with
`dry_run=false` is rejected unless the authenticated principal is an Admin.
At apply time the server re-checks that the plan's Google Ads customer belongs
to the Admin's server-owned account scope. A plan id is never treated as an
authorization capability.

### Tenant and account scope

`customer_ids` is an allowlist owned by the server configuration. Every Google
Ads client entry point checks it before OAuth or network traffic, including
GAQL reads, Keyword Planner, mutates, and recommendation actions. A caller's
`customer_id` argument cannot widen that scope. Include an MCC login account in
the allowlist only if that principal genuinely needs to query it.

Startup validation rejects a Google Ads customer ID assigned to two different
tenants. Principals within the same tenant may share an account, supporting an
Editor-to-Admin approval flow without allowing plans to cross tenant boundaries.

### Budget limits

`max_daily_budget` is set per principal/tenant and feeds the existing campaign
budget guard. Keep destructive operations in `blocked_operations` for tenants
that do not need them. These are mutation limits, not spend-ledger quotas: a
future monthly/cumulative limit should use durable storage plus live spend
reconciliation before supporting multiple replicas.

### Audit log

Mutation results are JSON Lines with timestamp, operation, customer, changes,
result, error, `tenant_id`, authenticated `actor`, and `role`. Mount the log
directory on durable storage and ship it to an append-only log service. Avoid
logging bearer or Google tokens; the implementation never puts them in an
audit record.

## Configure

1. Copy `config/remote.example.json` to an untracked path such as
   `/etc/mcp-google-ads/remote.json`.
2. Create a random bearer token with at least 32 characters and store it in the
   environment variable named by `bearer_token_env`. Alternatively store only
   its SHA-256 digest as `token_sha256`.
3. Mount Google OAuth client and refresh-token files read-only at the configured
   paths. Put the Google Ads developer token in the configured environment
   variable.
4. Set:

```bash
MCP_TRANSPORT=http
MCP_REMOTE_CONFIG=/etc/mcp-google-ads/remote.json
```

5. Start the binary and route only the configured HTTPS host to its listener.
   The MCP URL for principal `editor-1` is
   `https://mcp.example.com/mcp/editor-1` with header
   `Authorization: Bearer <token>`.

The unauthenticated `GET /healthz` endpoint returns only `ok`; it exposes no
tenant, account, or credential data.

## Reverse proxy requirements

- Terminate TLS with a trusted certificate and redirect plain HTTP to HTTPS.
- Preserve the original `Host`; it must match `allowed_hosts`.
- Do not expose the application listener directly to the internet.
- Disable proxy/body buffering for streaming responses and use timeouts long
  enough for SSE.
- Rate-limit authentication failures and MCP POST requests at the edge.
- Do not add a proxy-level shared credential that bypasses per-principal auth.

## Operational limits

Pending plans and MCP sessions are currently in memory. Run one replica (or use
sticky routing) and expect plans to disappear on restart. Before horizontal
scaling, move plans, sessions, approval state, and audit delivery to durable
shared storage. Static bearer credentials should be rotated through the secret
manager; OIDC/JWT verification with issuer, audience, expiry, and key rotation
is the recommended next authentication step for a large team.

## Verification

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

The test suite includes fail-closed checks for token configuration, HTTPS
public URLs, account scoping before HTTP traffic, Editor apply denial, and
Admin denial for out-of-scope plans.


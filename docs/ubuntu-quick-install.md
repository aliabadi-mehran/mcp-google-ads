# Ubuntu one-command installation

This installer turns a fresh Ubuntu server into a single-node Team Remote MCP
service. It builds the pinned source, creates an unprivileged system user,
installs a hardened systemd service, configures Nginx, obtains a Let's Encrypt
certificate, generates a 256-bit MCP bearer token, and verifies both local and
public health endpoints.

## Before you start

You need only:

- Ubuntu 22.04 or 24.04 with root or sudo access;
- a DNS name whose A/AAAA record already points to the server;
- a Google Ads developer token and 10-digit customer ID;
- either one `authorized_user` Google JSON file, or a Desktop OAuth
  `credentials.json` plus a `token.json` containing `refresh_token`.

Ports 80 and 443 must be reachable from the internet. The installer opens the
Nginx profile only when UFW is already active; cloud-provider firewalls remain
your responsibility.

Copy the Google files to the server first, for example:

```bash
scp credentials.json token.json root@SERVER_IP:/root/
```

## Install

SSH to the server and run:

```bash
curl -fsSL https://raw.githubusercontent.com/aliabadi-mehran/mcp-google-ads/main/scripts/install-ubuntu.sh \
  -o /tmp/install-mcp-google-ads.sh
sudo bash /tmp/install-mcp-google-ads.sh
```

The script asks only for the public domain, Let's Encrypt email, Google files,
developer token, customer ID, optional MCC ID, initial Admin identity, and a
daily campaign budget ceiling. It generates the MCP token and all service
configuration itself. Secret input is not written to shell history.

At completion it prints:

```text
MCP endpoint: https://mcp.example.com/mcp/admin
Bearer token: <generated-secret>
Health check: https://mcp.example.com/healthz
```

Use the endpoint as a Streamable HTTP MCP server and send the token as:

```text
Authorization: Bearer <generated-secret>
```

The token is also stored in `/root/.mcp-google-ads-admin-token` with mode 0600.

## What is installed

| Path | Purpose |
|---|---|
| `/opt/mcp-google-ads/bin/mcp-google-ads` | Server binary |
| `/etc/mcp-google-ads/remote.json` | Non-secret tenant, role and account policy |
| `/etc/mcp-google-ads/server.env` | Bearer and Google developer tokens |
| `/etc/mcp-google-ads/*.json` | Google OAuth material |
| `/var/log/mcp-google-ads/audit.jsonl` | Actor-attributed mutation audit log |
| `/etc/systemd/system/mcp-google-ads.service` | Hardened service definition |
| `/etc/nginx/sites-available/mcp-google-ads` | HTTPS reverse proxy |

All secret files are readable only by root and the dedicated
`mcp-google-ads` service account. The application listens only on
`127.0.0.1:8080`; Nginx is the public TLS boundary.

## Operate and troubleshoot

```bash
sudo systemctl status mcp-google-ads
sudo journalctl -u mcp-google-ads -f
curl https://YOUR_DOMAIN/healthz
sudo nginx -t
sudo certbot renew --dry-run
```

Run the installer again to rebuild and replace the binary and configuration.
It creates a new Admin bearer token, so update MCP clients after reinstalling.

For multiple users and Analyst/Editor/Admin policies, edit
`/etc/mcp-google-ads/remote.json` and `/etc/mcp-google-ads/server.env`, following
[`remote-deployment.md`](remote-deployment.md), then restart the service:

```bash
sudo systemctl restart mcp-google-ads
```

## Production boundary

This installer deliberately creates a single-node deployment. Pending approval
plans and MCP sessions are in memory, so do not put multiple replicas behind a
load balancer without shared durable plan/session storage. Static bearer tokens
are appropriate for a small controlled team; use an identity-aware proxy or
OIDC/JWT integration for a larger organization.


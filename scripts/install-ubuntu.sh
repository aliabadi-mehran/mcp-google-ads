#!/usr/bin/env bash
# Interactive production installer for a fresh Ubuntu server.

set -Eeuo pipefail
umask 027

REPO_URL="${MCP_GOOGLE_ADS_REPO_URL:-https://github.com/aliabadi-mehran/mcp-google-ads.git}"
REPO_REF="${MCP_GOOGLE_ADS_REPO_REF:-main}"
APP_USER="mcp-google-ads"
INSTALL_ROOT="/opt/mcp-google-ads"
CONFIG_ROOT="/etc/mcp-google-ads"
LOG_ROOT="/var/log/mcp-google-ads"
ENV_FILE="$CONFIG_ROOT/server.env"
REMOTE_CONFIG="$CONFIG_ROOT/remote.json"
TOKEN_BACKUP="/root/.mcp-google-ads-admin-token"

fail() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

prompt() {
    local label="$1" default_value="${2:-}" result
    if [[ -n "$default_value" ]]; then
        read -r -p "$label [$default_value]: " result
        printf '%s' "${result:-$default_value}"
    else
        read -r -p "$label: " result
        printf '%s' "$result"
    fi
}

[[ ${EUID:-$(id -u)} -eq 0 ]] || fail "run this installer with sudo"
[[ -r /etc/os-release ]] || fail "cannot identify this Linux distribution"
# shellcheck disable=SC1091
source /etc/os-release
[[ "${ID:-}" == "ubuntu" ]] || fail "this installer currently supports Ubuntu only"

printf '\nGoogle Ads Team Remote MCP installer\n'
printf '%s\n' '------------------------------------'
printf '%s\n' 'Before continuing, point a DNS A/AAAA record at this server and upload your Google OAuth JSON file.'
printf '%s\n\n' 'The installer generates the MCP bearer token automatically.'

DOMAIN="$(prompt 'Public DNS name (for example mcp.example.com)')"
[[ "$DOMAIN" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ ]] || fail "invalid DNS name"

LE_EMAIL="$(prompt "Email for Let's Encrypt notices")"
[[ "$LE_EMAIL" == *@*.* ]] || fail "invalid email address"

CREDENTIALS_SOURCE="$(prompt 'Path to the uploaded Google OAuth credentials JSON')"
[[ -f "$CREDENTIALS_SOURCE" ]] || fail "credentials file not found: $CREDENTIALS_SOURCE"

CREDENTIAL_KIND="$(python3 - "$CREDENTIALS_SOURCE" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle:
    value = json.load(handle)
if value.get("type") == "authorized_user":
    print("authorized_user")
elif value.get("type") == "service_account":
    print("service_account")
elif "installed" in value:
    print("installed")
elif "web" in value:
    print("web")
else:
    print("unknown")
PY
)" || fail "credentials file is not valid JSON"
[[ "$CREDENTIAL_KIND" != "unknown" ]] || fail "unsupported Google credentials format"

TOKEN_SOURCE=""
if [[ "$CREDENTIAL_KIND" == "installed" || "$CREDENTIAL_KIND" == "web" ]]; then
    TOKEN_SOURCE="$(prompt 'Path to token.json containing the Google refresh_token')"
    [[ -f "$TOKEN_SOURCE" ]] || fail "token file not found: $TOKEN_SOURCE"
    python3 - "$TOKEN_SOURCE" <<'PY' || fail "token.json must contain refresh_token"
import json, sys
with open(sys.argv[1], encoding="utf-8") as handle:
    value = json.load(handle)
if not value.get("refresh_token"):
    raise SystemExit(1)
PY
fi

read -r -s -p 'Google Ads developer token: ' DEVELOPER_TOKEN
printf '\n'
[[ "$DEVELOPER_TOKEN" =~ ^[A-Za-z0-9._~-]+$ ]] || fail "developer token contains unsupported characters"

CUSTOMER_ID="$(prompt 'Google Ads customer ID (digits or 123-456-7890)')"
CUSTOMER_ID="${CUSTOMER_ID//-/}"
[[ "$CUSTOMER_ID" =~ ^[0-9]{10}$ ]] || fail "customer ID must contain exactly 10 digits"

LOGIN_CUSTOMER_ID="$(prompt 'MCC login customer ID; leave empty when not using an MCC')"
LOGIN_CUSTOMER_ID="${LOGIN_CUSTOMER_ID//-/}"
[[ -z "$LOGIN_CUSTOMER_ID" || "$LOGIN_CUSTOMER_ID" =~ ^[0-9]{10}$ ]] || fail "MCC ID must contain exactly 10 digits"

ADMIN_SUBJECT="$(prompt 'Initial Admin name or email' "admin@$DOMAIN")"
[[ -n "$ADMIN_SUBJECT" ]] || fail "Admin identity cannot be empty"

MAX_DAILY_BUDGET="$(prompt 'Maximum daily campaign budget' '50.0')"
[[ "$MAX_DAILY_BUDGET" =~ ^[0-9]+([.][0-9]+)?$ ]] || fail "budget must be a positive number"
python3 - "$MAX_DAILY_BUDGET" <<'PY' || fail "budget must be greater than zero"
import sys
raise SystemExit(0 if float(sys.argv[1]) > 0 else 1)
PY

printf '\nInstalling packages and building the server. This can take several minutes...\n'
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl git jq nginx certbot python3 python3-certbot-nginx build-essential pkg-config libssl-dev openssl

if [[ ! -x /root/.cargo/bin/cargo ]]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
fi
export PATH="/root/.cargo/bin:$PATH"
rustup update stable

BUILD_ROOT="$(mktemp -d)"
trap 'rm -rf "$BUILD_ROOT"' EXIT
git clone --depth 1 --branch "$REPO_REF" "$REPO_URL" "$BUILD_ROOT/source"
cargo build --locked --release --manifest-path "$BUILD_ROOT/source/Cargo.toml"

if ! id "$APP_USER" >/dev/null 2>&1; then
    useradd --system --create-home --home-dir "/var/lib/$APP_USER" --shell /usr/sbin/nologin "$APP_USER"
fi
install -d -m 0755 "$INSTALL_ROOT/bin"
install -d -m 0750 -o root -g "$APP_USER" "$CONFIG_ROOT"
install -d -m 0750 -o "$APP_USER" -g "$APP_USER" "$LOG_ROOT"
install -m 0755 "$BUILD_ROOT/source/target/release/mcp-google-ads" "$INSTALL_ROOT/bin/mcp-google-ads"

install_secret() {
    local source_path="$1" destination_path="$2"
    if [[ "$(readlink -f "$source_path")" != "$(readlink -m "$destination_path")" ]]; then
        install -m 0640 -o root -g "$APP_USER" "$source_path" "$destination_path"
    else
        chown root:"$APP_USER" "$destination_path"
        chmod 0640 "$destination_path"
    fi
}

install_secret "$CREDENTIALS_SOURCE" "$CONFIG_ROOT/credentials.json"
if [[ -n "$TOKEN_SOURCE" ]]; then
    install_secret "$TOKEN_SOURCE" "$CONFIG_ROOT/token.json"
elif [[ "$CREDENTIAL_KIND" == "authorized_user" ]]; then
    install_secret "$CREDENTIALS_SOURCE" "$CONFIG_ROOT/token.json"
else
    printf '{}\n' > "$CONFIG_ROOT/token.json"
    chown root:"$APP_USER" "$CONFIG_ROOT/token.json"
    chmod 0640 "$CONFIG_ROOT/token.json"
fi

MCP_TOKEN="$(openssl rand -hex 32)"
{
    printf 'MCP_INITIAL_ADMIN_TOKEN=%s\n' "$MCP_TOKEN"
    printf 'GOOGLE_ADS_DEVELOPER_TOKEN_REMOTE=%s\n' "$DEVELOPER_TOKEN"
    printf 'RUST_LOG=info\n'
} > "$ENV_FILE"
chown root:"$APP_USER" "$ENV_FILE"
chmod 0640 "$ENV_FILE"

jq -n \
    --arg domain "$DOMAIN" \
    --arg subject "$ADMIN_SUBJECT" \
    --arg customer "$CUSTOMER_ID" \
    --arg login_customer "$LOGIN_CUSTOMER_ID" \
    --argjson budget "$MAX_DAILY_BUDGET" \
    '{
      listen: "127.0.0.1:8080",
      public_base_url: ("https://" + $domain),
      allowed_hosts: [$domain],
      allowed_origins: [],
      principals: [{
        id: "admin",
        tenant_id: "default",
        subject: $subject,
        role: "admin",
        bearer_token_env: "MCP_INITIAL_ADMIN_TOKEN",
        customer_ids: [$customer],
        default_customer_id: $customer,
        login_customer_id: (if $login_customer == "" then null else $login_customer end),
        credentials_path: "/etc/mcp-google-ads/credentials.json",
        token_path: "/etc/mcp-google-ads/token.json",
        developer_token_env: "GOOGLE_ADS_DEVELOPER_TOKEN_REMOTE",
        audit_log: "/var/log/mcp-google-ads/audit.jsonl",
        max_daily_budget: $budget,
        blocked_operations: []
      }]
    }' > "$REMOTE_CONFIG"
chown root:"$APP_USER" "$REMOTE_CONFIG"
chmod 0640 "$REMOTE_CONFIG"

cat > /etc/systemd/system/mcp-google-ads.service <<EOF
[Unit]
Description=Google Ads Team Remote MCP
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$APP_USER
Group=$APP_USER
Environment=MCP_TRANSPORT=http
Environment=MCP_REMOTE_CONFIG=$REMOTE_CONFIG
EnvironmentFile=$ENV_FILE
ExecStart=$INSTALL_ROOT/bin/mcp-google-ads
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true
ProtectHome=true
ProtectSystem=strict
ReadWritePaths=$LOG_ROOT
CapabilityBoundingSet=
LockPersonality=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
UMask=0027

[Install]
WantedBy=multi-user.target
EOF

cat > /etc/nginx/sites-available/mcp-google-ads <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name $DOMAIN;

    client_max_body_size 2m;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_set_header Connection "";
        proxy_buffering off;
        proxy_read_timeout 3600s;
    }
}
EOF
ln -sfn /etc/nginx/sites-available/mcp-google-ads /etc/nginx/sites-enabled/mcp-google-ads
nginx -t

systemctl daemon-reload
systemctl enable --now mcp-google-ads
systemctl enable --now nginx
curl --fail --silent --show-error --retry 10 --retry-delay 2 http://127.0.0.1:8080/healthz >/dev/null || {
    journalctl -u mcp-google-ads --no-pager -n 50 >&2
    fail "the MCP service did not pass its local health check"
}

if command -v ufw >/dev/null 2>&1 && ufw status | grep -q '^Status: active'; then
    ufw allow 'Nginx Full'
fi

certbot --nginx --non-interactive --agree-tos --redirect --email "$LE_EMAIL" -d "$DOMAIN"
curl --fail --silent --show-error --retry 5 "https://$DOMAIN/healthz" >/dev/null || fail "public HTTPS health check failed"

printf '%s\n' "$MCP_TOKEN" > "$TOKEN_BACKUP"
chmod 0600 "$TOKEN_BACKUP"

printf '\nInstallation complete.\n'
printf 'MCP endpoint: https://%s/mcp/admin\n' "$DOMAIN"
printf 'Bearer token: %s\n' "$MCP_TOKEN"
printf 'A root-only copy of the token is stored at %s\n' "$TOKEN_BACKUP"
printf 'Health check: https://%s/healthz\n' "$DOMAIN"
printf 'Service status: systemctl status mcp-google-ads\n'
printf '\nTreat the bearer token like a password. Do not paste it into tickets or commit it to Git.\n'


//! Multi-tenant Streamable HTTP gateway for remote MCP deployments.
//!
//! TLS is intentionally terminated by a production reverse proxy/ingress. The
//! application requires an `https://` public URL and uses a separate MCP
//! service/session manager per principal so session ids cannot cross users.

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    config::{AccessContext, AccessRole, Config},
    error::{McpGoogleAdsError, Result},
    GoogleAdsMcp,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteServerConfig {
    pub listen: String,
    pub public_base_url: String,
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    pub principals: Vec<RemotePrincipalConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteRole {
    Analyst,
    Editor,
    Admin,
}

impl From<RemoteRole> for AccessRole {
    fn from(value: RemoteRole) -> Self {
        match value {
            RemoteRole::Analyst => Self::Analyst,
            RemoteRole::Editor => Self::Editor,
            RemoteRole::Admin => Self::Admin,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePrincipalConfig {
    /// Stable, non-secret route component: `/mcp/{id}`.
    pub id: String,
    pub tenant_id: String,
    pub subject: String,
    pub role: RemoteRole,
    /// Preferred: environment variable containing the bearer token.
    pub bearer_token_env: Option<String>,
    /// Alternative: lowercase SHA-256 of the bearer token. The token itself is
    /// never stored in the repository or configuration file.
    pub token_sha256: Option<String>,
    pub customer_ids: Vec<String>,
    pub default_customer_id: String,
    pub login_customer_id: Option<String>,
    pub credentials_path: PathBuf,
    pub token_path: PathBuf,
    pub developer_token_env: String,
    pub audit_log: PathBuf,
    pub max_daily_budget: f64,
    #[serde(default)]
    pub blocked_operations: Vec<String>,
}

#[derive(Clone)]
struct ExpectedTokenHash(Arc<[u8; 32]>);

fn sha256_bytes(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn parse_hash(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(McpGoogleAdsError::Config(
            "token_sha256 must be exactly 64 hexadecimal characters".to_string(),
        ));
    }
    let mut output = [0_u8; 32];
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for (index, pair) in pairs.iter().enumerate() {
        let pair = std::str::from_utf8(pair)
            .map_err(|_| McpGoogleAdsError::Config("invalid token_sha256".to_string()))?;
        output[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| McpGoogleAdsError::Config("invalid token_sha256".to_string()))?;
    }
    Ok(output)
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut difference = 0_u8;
    for index in 0..32 {
        difference |= left[index] ^ right[index];
    }
    difference == 0
}

impl RemotePrincipalConfig {
    fn expected_token_hash(&self) -> Result<ExpectedTokenHash> {
        match (&self.bearer_token_env, &self.token_sha256) {
            (Some(_), Some(_)) | (None, None) => Err(McpGoogleAdsError::Config(format!(
                "principal '{}' must set exactly one of bearer_token_env or token_sha256",
                self.id
            ))),
            (Some(key), None) => {
                let token = std::env::var(key).map_err(|_| {
                    McpGoogleAdsError::Config(format!(
                        "principal '{}' bearer token environment variable '{}' is missing",
                        self.id, key
                    ))
                })?;
                if token.len() < 32 {
                    return Err(McpGoogleAdsError::Config(format!(
                        "principal '{}' bearer token must contain at least 32 characters",
                        self.id
                    )));
                }
                Ok(ExpectedTokenHash(Arc::new(sha256_bytes(&token))))
            }
            (None, Some(hash)) => Ok(ExpectedTokenHash(Arc::new(parse_hash(hash)?))),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(McpGoogleAdsError::Config(
                "principal id must contain only ASCII letters, digits, '-' or '_'".to_string(),
            ));
        }
        if self.tenant_id.is_empty() || self.subject.is_empty() {
            return Err(McpGoogleAdsError::Config(format!(
                "principal '{}' requires non-empty tenant_id and subject",
                self.id
            )));
        }
        if self.customer_ids.is_empty()
            || !self
                .customer_ids
                .iter()
                .any(|id| id.replace('-', "") == self.default_customer_id.replace('-', ""))
        {
            return Err(McpGoogleAdsError::Config(format!(
                "principal '{}' default_customer_id must be present in customer_ids",
                self.id
            )));
        }
        if !self.max_daily_budget.is_finite() || self.max_daily_budget <= 0.0 {
            return Err(McpGoogleAdsError::Config(format!(
                "principal '{}' max_daily_budget must be positive and finite",
                self.id
            )));
        }
        let _ = self.expected_token_hash()?;
        Ok(())
    }

    fn runtime_config(&self, base: &Config) -> Result<Config> {
        let developer_token = std::env::var(&self.developer_token_env).map_err(|_| {
            McpGoogleAdsError::Config(format!(
                "principal '{}' developer token environment variable '{}' is missing",
                self.id, self.developer_token_env
            ))
        })?;
        let role: AccessRole = self.role.into();
        let mut config = base.clone();
        config.google.credentials_path = self.credentials_path.clone();
        config.google.token_path = self.token_path.clone();
        config.ads.developer_token = developer_token;
        config.ads.customer_id = self.default_customer_id.clone();
        config.ads.login_customer_id = self.login_customer_id.clone();
        config.safety.log_file = self.audit_log.clone();
        config.safety.max_daily_budget = self.max_daily_budget;
        config.safety.blocked_operations = self.blocked_operations.clone();
        config.read_only = matches!(role, AccessRole::Analyst);
        config.access = AccessContext {
            tenant_id: self.tenant_id.clone(),
            subject: self.subject.clone(),
            role,
            allowed_customer_ids: self.customer_ids.clone(),
        };
        Ok(config)
    }
}

impl RemoteServerConfig {
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let config: Self = serde_json::from_slice(&bytes)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.public_base_url.starts_with("https://") {
            return Err(McpGoogleAdsError::Config(
                "remote public_base_url must start with https://; terminate TLS at the reverse proxy"
                    .to_string(),
            ));
        }
        if self.allowed_hosts.is_empty() {
            return Err(McpGoogleAdsError::Config(
                "remote allowed_hosts cannot be empty".to_string(),
            ));
        }
        if self.principals.is_empty() {
            return Err(McpGoogleAdsError::Config(
                "remote configuration requires at least one principal".to_string(),
            ));
        }
        let _: SocketAddr = self.listen.parse().map_err(|_| {
            McpGoogleAdsError::Config("remote listen must be a socket address".to_string())
        })?;
        let mut ids = HashSet::new();
        let mut customer_tenants: HashMap<String, String> = HashMap::new();
        for principal in &self.principals {
            principal.validate()?;
            if !ids.insert(&principal.id) {
                return Err(McpGoogleAdsError::Config(format!(
                    "duplicate principal id '{}'",
                    principal.id
                )));
            }
            for customer_id in &principal.customer_ids {
                let normalized = customer_id.replace('-', "");
                if let Some(existing_tenant) = customer_tenants.get(&normalized) {
                    if existing_tenant != &principal.tenant_id {
                        return Err(McpGoogleAdsError::Config(format!(
                            "customer_id '{}' cannot be shared by tenants '{}' and '{}'",
                            customer_id, existing_tenant, principal.tenant_id
                        )));
                    }
                } else {
                    customer_tenants.insert(normalized, principal.tenant_id.clone());
                }
            }
        }
        Ok(())
    }
}

async fn require_bearer(
    State(expected): State<ExpectedTokenHash>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if presented.is_some_and(|token| {
        token.len() >= 32 && constant_time_eq(&sha256_bytes(token), expected.0.as_ref())
    }) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"mcp-google-ads\"")],
            "unauthorized",
        )
            .into_response()
    }
}

pub fn build_router(remote: &RemoteServerConfig, base: &Config) -> Result<Router> {
    remote.validate()?;
    let mut router = Router::new().route("/healthz", get(|| async { (StatusCode::OK, "ok") }));

    for principal in &remote.principals {
        let expected = principal.expected_token_hash()?;
        let config = principal.runtime_config(base)?;
        let factory_config = config.clone();
        let http_config = StreamableHttpServerConfig::default()
            .with_allowed_hosts(remote.allowed_hosts.clone())
            .with_allowed_origins(remote.allowed_origins.clone())
            .with_json_response(true);
        let service: StreamableHttpService<GoogleAdsMcp, LocalSessionManager> =
            StreamableHttpService::new(
                move || GoogleAdsMcp::new(factory_config.clone()).map_err(std::io::Error::other),
                Default::default(),
                http_config,
            );
        let path = format!("/mcp/{}", principal.id);
        let protected = Router::new()
            .nest_service(&path, service)
            .route_layer(middleware::from_fn_with_state(expected, require_bearer));
        router = router.merge(protected);
    }
    Ok(router)
}

pub async fn serve(base: Config) -> Result<()> {
    let path = std::env::var("MCP_REMOTE_CONFIG").map_err(|_| {
        McpGoogleAdsError::Config(
            "MCP_REMOTE_CONFIG must point to the remote JSON configuration".to_string(),
        )
    })?;
    let remote = RemoteServerConfig::from_path(path)?;
    let address: SocketAddr = remote.listen.parse().map_err(|_| {
        McpGoogleAdsError::Config("remote listen must be a socket address".to_string())
    })?;
    let router = build_router(&remote, &base)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, public_base_url = %remote.public_base_url, "remote MCP listening behind HTTPS proxy");
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal() -> RemotePrincipalConfig {
        RemotePrincipalConfig {
            id: "editor-1".to_string(),
            tenant_id: "tenant-a".to_string(),
            subject: "editor@example.test".to_string(),
            role: RemoteRole::Editor,
            bearer_token_env: None,
            token_sha256: Some("00".repeat(32)),
            customer_ids: vec!["123-456-7890".to_string()],
            default_customer_id: "1234567890".to_string(),
            login_customer_id: None,
            credentials_path: PathBuf::from("/run/secrets/google-credentials.json"),
            token_path: PathBuf::from("/run/secrets/google-token.json"),
            developer_token_env: "TEST_DEV_TOKEN".to_string(),
            audit_log: PathBuf::from("/var/log/mcp/audit.jsonl"),
            max_daily_budget: 50.0,
            blocked_operations: Vec::new(),
        }
    }

    #[test]
    fn parses_and_compares_token_hashes() {
        let hash = sha256_bytes("a sufficiently long bearer token value");
        let encoded = hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(parse_hash(&encoded).unwrap(), hash);
        assert!(constant_time_eq(&hash, &hash));
        assert!(!constant_time_eq(&hash, &sha256_bytes("another token")));
    }

    #[test]
    fn principal_requires_scoped_default_customer() {
        let mut value = principal();
        assert!(value.validate().is_ok());
        value.default_customer_id = "9999999999".to_string();
        assert!(value.validate().is_err());
    }

    #[test]
    fn remote_requires_https_public_url() {
        let config = RemoteServerConfig {
            listen: "127.0.0.1:8080".to_string(),
            public_base_url: "http://example.test".to_string(),
            allowed_hosts: vec!["example.test".to_string()],
            allowed_origins: Vec::new(),
            principals: vec![principal()],
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn customer_account_cannot_cross_tenants() {
        let first = principal();
        let mut second = principal();
        second.id = "admin-2".to_string();
        second.tenant_id = "tenant-b".to_string();
        let config = RemoteServerConfig {
            listen: "127.0.0.1:8080".to_string(),
            public_base_url: "https://example.test".to_string(),
            allowed_hosts: vec!["example.test".to_string()],
            allowed_origins: Vec::new(),
            principals: vec![first, second],
        };
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("cannot be shared by tenants"));
    }
}


use anyhow::{Context, Result, bail};
use digest_auth::AuthContext;
use reqwest::{Client, Method, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tracing::{debug, info, warn};

const CURL_DEBUG_DIR: &str = "/tmp/marklogic-tui-curl";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
pub enum AuthType {
    #[default]
    #[serde(rename = "digest")]
    Digest,
    #[serde(rename = "basic")]
    Basic,
    #[serde(rename = "digestbasic")]
    DigestBasic,
    #[serde(rename = "application-level")]
    ApplicationLevel,
}

impl AuthType {
    pub const VARIANTS: &'static [AuthType] = &[
        AuthType::Digest,
        AuthType::Basic,
        AuthType::DigestBasic,
        AuthType::ApplicationLevel,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            AuthType::Digest => "digest",
            AuthType::Basic => "basic",
            AuthType::DigestBasic => "digestbasic",
            AuthType::ApplicationLevel => "application-level",
        }
    }
}

impl std::fmt::Display for AuthType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AuthType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "digest" => Ok(AuthType::Digest),
            "basic" => Ok(AuthType::Basic),
            "digestbasic" => Ok(AuthType::DigestBasic),
            "application-level" | "applicationlevel" | "application" => {
                Ok(AuthType::ApplicationLevel)
            }
            _ => Err(format!("Unknown auth type: {}", s)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppServerEndpoint {
    pub name: String,
    pub port: u16,
    pub secure: bool,
    #[serde(default = "default_app_server_group")]
    pub group: String,
    #[serde(default)]
    pub content_database: Option<String>,
    #[serde(default)]
    pub modules_database: Option<String>,
    #[serde(default)]
    pub favorite: bool,
}

pub fn default_app_server_group() -> String {
    "Default".to_string()
}

impl AppServerEndpoint {
    pub fn app_services_default() -> Self {
        Self {
            name: "App-Services".to_string(),
            port: 8000,
            secure: false,
            group: default_app_server_group(),
            content_database: Some("Documents".to_string()),
            modules_database: Some("Modules".to_string()),
            favorite: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub host: String,
    pub username: String,
    pub password: String,
    pub port: u16,
    pub secure: bool,
    #[serde(default)]
    pub auth_type: AuthType,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub app_servers: Vec<AppServerEndpoint>,
    // Legacy field for backward compatibility migration
    #[serde(default)]
    pub uri: Option<String>,
}

impl ServerConfig {
    pub fn base_url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{}://{}:{}", scheme, self.host, self.port)
    }

    pub fn manage_url(&self, path: &str) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{}://{}:8002{}", scheme, self.host, path)
    }
}

/// Clean up old curl debug files and write the current one.
/// Returns the path to the written file, or None if writing failed.
fn write_curl_debug_file(
    method: &Method,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    username: &str,
    password: &str,
    auth_type: &AuthType,
    insecure: bool,
) -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(CURL_DEBUG_DIR);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::create_dir_all(&dir);

    let mut cmd = if insecure {
        format!("curl -k -X {} '{}'", method, url)
    } else {
        format!("curl -X {} '{}'", method, url)
    };

    for (k, v) in headers {
        cmd.push_str(&format!(" \n  -H '{}: {}'", k, v));
    }

    match auth_type {
        AuthType::Basic => {
            cmd.push_str(&format!(" \n  -u '{}:{}'", username, password));
        }
        AuthType::Digest | AuthType::DigestBasic => {
            cmd.push_str(&format!(" \n  --digest -u '{}:{}'", username, password));
        }
        AuthType::ApplicationLevel => {}
    }

    if let Some(body) = body {
        cmd.push_str(&format!(" \n  -d '{}'", body));
    }

    cmd.push('\n');

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let file = dir.join(format!("{}-{}.sh", method, ts));
    match fs::write(&file, cmd) {
        Ok(_) => Some(file),
        Err(_) => None,
    }
}

#[derive(Debug, Clone)]
pub struct MarkLogicClient {
    pub server: ServerConfig,
    pub database: Option<String>,
    pub modules_database: Option<String>,
    client: Client,
    download_counter: Option<Arc<AtomicU64>>,
    download_progress: Option<Arc<AtomicU64>>,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub uri: String,
    pub collections: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DocumentDetail {
    pub uri: String,
    pub collections: Vec<String>,
    pub quality: Option<i64>,
    pub permissions: Vec<String>,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct DocumentMetadata {
    pub collections: Vec<String>,
    pub quality: Option<i64>,
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PagedResults {
    pub results: Vec<SearchResult>,
    pub total: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ModuleSearchResult {
    pub uris: Vec<String>,
    pub total: Option<usize>,
}

impl MarkLogicClient {
    pub fn new(server: ServerConfig) -> Result<Self> {
        let client = if server.insecure {
            info!(host = %server.host, port = server.port, "Creating HTTP client with insecure TLS (skipping certificate validation)");
            reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .context("Failed to build HTTP client with insecure TLS")?
        } else {
            Client::new()
        };
        Ok(Self {
            server,
            database: None,
            modules_database: None,
            client,
            download_counter: None,
            download_progress: None,
        })
    }

    pub fn set_database(&mut self, db: String) {
        self.database = Some(db);
    }

    pub fn set_modules_database(&mut self, db: String) {
        self.modules_database = Some(db);
    }

    fn system_url(&self, path: &str) -> String {
        format!("{}{}", self.server.base_url(), path)
    }

    fn log_request_attempt(&self, phase: &str, method: &Method, req: &RequestBuilder) {
        let Some(cloned) = req.try_clone() else {
            warn!(
                phase,
                method = %method,
                "unable to clone request for URL logging"
            );
            return;
        };

        match cloned.build() {
            Ok(request) => {
                let url = request.url().to_string();
                let query = request.url().query().unwrap_or("").to_string();
                let headers: Vec<(String, String)> = request
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
                    .collect();
                let body = request
                    .body()
                    .and_then(|b| b.as_bytes())
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .map(String::from);

                debug!(
                    phase,
                    method = %method,
                    url = %url,
                    query = query,
                    "sending MarkLogic request"
                );

                // Reproduction scripts embed credentials and cost a directory
                // rewrite per request, so they are strictly opt-in.
                if std::env::var_os("MARKLOGIC_TUI_CURL_DEBUG").is_some()
                    && let Some(curl_file) = write_curl_debug_file(
                        method,
                        &url,
                        &headers,
                        body.as_deref(),
                        &self.server.username,
                        &self.server.password,
                        &self.server.auth_type,
                        self.server.insecure,
                    )
                {
                    info!(
                        curl_repro = %curl_file.display(),
                        "curl command written for debugging"
                    );
                }
            }
            Err(e) => {
                warn!(
                    phase,
                    method = %method,
                    error = %e,
                    "unable to build request for URL logging"
                );
            }
        }
    }

    /// Unified request handler that dispatches to the appropriate auth strategy
    /// based on the server's configured auth_type.
    async fn request(
        &self,
        method: Method,
        url: &str,
        configure: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response> {
        match self.server.auth_type {
            AuthType::Digest => self.request_digest(method, url, configure).await,
            AuthType::Basic => self.request_basic(method, url, configure).await,
            AuthType::DigestBasic => {
                match self.request_digest(method.clone(), url, &configure).await {
                    Ok(resp) => Ok(resp),
                    Err(_) => self.request_basic(method, url, configure).await,
                }
            }
            AuthType::ApplicationLevel => self.request_no_auth(method, url, configure).await,
        }
    }

    async fn request_no_auth(
        &self,
        method: Method,
        url: &str,
        configure: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response> {
        let req = configure(self.client.request(method.clone(), url));
        self.log_request_attempt("application-level", &method, &req);
        let resp = req.send().await?;
        debug!(
            phase = "application-level",
            method = %method,
            url,
            status = %resp.status(),
            "received MarkLogic response"
        );

        if !resp.status().is_success() {
            let status = resp.status();
            let body = self.error_response_body(resp).await;
            bail!("{} {} failed with status {}: {}", method, url, status, body);
        }

        Ok(resp)
    }

    async fn request_basic(
        &self,
        method: Method,
        url: &str,
        configure: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response> {
        let req = configure(
            self.client
                .request(method.clone(), url)
                .basic_auth(&self.server.username, Some(&self.server.password)),
        );
        self.log_request_attempt("basic-auth", &method, &req);
        let resp = req.send().await?;
        debug!(
            phase = "basic-auth",
            method = %method,
            url,
            status = %resp.status(),
            "received MarkLogic response"
        );

        if !resp.status().is_success() {
            let status = resp.status();
            let body = self.error_response_body(resp).await;
            bail!("{} {} failed with status {}: {}", method, url, status, body);
        }

        Ok(resp)
    }

    async fn error_response_body(&self, resp: Response) -> String {
        let bytes = self.read_response_body(resp).await.unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn read_response_body(&self, mut resp: Response) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            self.count_download(chunk.len());
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    fn count_download(&self, bytes: usize) {
        if let Some(counter) = &self.download_counter {
            counter.fetch_add(bytes as u64, Ordering::Relaxed);
        }
        if let Some(counter) = &self.download_progress {
            counter.fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }

    /// A separate collector client shares a counter across all host requests.
    pub(crate) fn with_download_counter(mut self, counter: Arc<AtomicU64>) -> Self {
        self.download_counter = Some(counter);
        self.download_progress = None;
        self
    }

    pub(crate) fn with_download_progress(mut self, progress: Arc<AtomicU64>) -> Self {
        self.download_progress = Some(progress);
        self
    }

    /// Logs use the App Server's default database and do not depend on the
    /// database/modules selection in the document or query panes.
    pub(crate) async fn logs_eval(&self, script: &str, vars: &Value) -> Result<Value> {
        self.logs_eval_with_timeout(script, vars, std::time::Duration::from_secs(30))
            .await
    }

    /// Full log reads can be much larger than inventory and scan requests.
    pub(crate) async fn logs_eval_download(&self, script: &str, vars: &Value) -> Result<Value> {
        self.logs_eval_with_timeout(script, vars, std::time::Duration::from_secs(600))
            .await
    }

    async fn logs_eval_with_timeout(
        &self,
        script: &str,
        vars: &Value,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        let body = format!(
            "xquery={}&vars={}",
            urlencoding::encode(script),
            urlencoding::encode(&serde_json::to_string(vars)?)
        );
        let url = self.system_url("/v1/eval");
        let resp = self
            .request(Method::POST, &url, |r| {
                r.timeout(timeout)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body.clone())
            })
            .await
            .context("Log extraction failed (requires eval and filesystem privileges)")?;
        let status = resp.status();
        let bytes = self
            .read_response_body(resp)
            .await
            .context("Reading log response failed")?;
        let text = std::str::from_utf8(&bytes)?;
        if !status.is_success() {
            bail!("Log extraction returned {}: {}", status, text);
        }
        if let Ok(value) = serde_json::from_str(text) {
            return Ok(value);
        }
        let parts = parse_eval_response_parts(text)?;
        if parts.len() != 1 {
            bail!("Expected one JSON log response, got {} parts", parts.len());
        }
        serde_json::from_str(&parts[0]).context("Invalid JSON log response")
    }

    /// Perform a request with digest authentication.
    /// First sends without auth to get the WWW-Authenticate challenge,
    /// then retries with the computed digest header.
    async fn request_digest(
        &self,
        method: Method,
        url: &str,
        configure: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response> {
        // First request to get the digest challenge
        let req = configure(self.client.request(method.clone(), url));
        self.log_request_attempt("initial", &method, &req);
        let resp = req.send().await?;
        debug!(
            phase = "initial",
            method = %method,
            url,
            status = %resp.status(),
            "received MarkLogic response"
        );

        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        // Extract WWW-Authenticate header
        let www_auth = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        // Include digest challenge bodies in the collector's transfer count.
        let _ = self.read_response_body(resp).await?;

        let Some(www_auth) = www_auth else {
            bail!("Server returned 401 but no WWW-Authenticate header");
        };

        // Parse the URI path for digest auth
        let uri_path = url
            .strip_prefix(&format!("{}://", url.split("://").next().unwrap_or("http")))
            .and_then(|s| s.find('/').map(|i| &s[i..]))
            .unwrap_or("/");
        // Strip query params for the path used in digest, but keep full URI in request
        let digest_uri = uri_path.split('?').next().unwrap_or(uri_path);

        let method_enum = match method.as_str() {
            "POST" => digest_auth::HttpMethod::POST,
            "PUT" => digest_auth::HttpMethod::PUT,
            "PATCH" => digest_auth::HttpMethod::PATCH,
            "DELETE" => digest_auth::HttpMethod::DELETE,
            _ => digest_auth::HttpMethod::GET,
        };

        let context = AuthContext::new_with_method(
            &self.server.username,
            &self.server.password,
            digest_uri,
            Option::<&[u8]>::None,
            method_enum,
        );

        let mut prompt = digest_auth::parse(&www_auth)
            .map_err(|e| anyhow::anyhow!("Failed to parse digest challenge: {:?}", e))?;
        let auth_header = prompt
            .respond(&context)
            .map_err(|e| anyhow::anyhow!("Failed to compute digest response: {:?}", e))?
            .to_header_string();

        // Retry with digest auth
        let req = configure(self.client.request(method.clone(), url))
            .header("Authorization", auth_header);
        self.log_request_attempt("digest-auth-retry", &method, &req);
        let resp = req.send().await?;
        debug!(
            phase = "digest-auth-retry",
            method = %method,
            url,
            status = %resp.status(),
            "received MarkLogic response"
        );

        if !resp.status().is_success() {
            let status = resp.status();
            let body = self.error_response_body(resp).await;
            bail!("{} {} failed with status {}: {}", method, url, status, body);
        }

        Ok(resp)
    }

    #[allow(dead_code)]
    /// List databases via the manage API (port 8002)
    pub async fn list_databases(&self) -> Result<Vec<String>> {
        let url = self.server.manage_url("/manage/v2/databases?format=json");
        let resp = self
            .request(Method::GET, &url, |r| r)
            .await
            .context("Failed to list databases")?;
        let body: Value = resp.json().await?;
        let dbs = body["database-default-list"]["list-items"]["list-item"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item["nameref"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(dbs)
    }

    fn parse_app_servers(body: &Value) -> Result<Vec<AppServerEndpoint>> {
        if let Some(entries) = body.pointer("/set/servers").and_then(Value::as_array) {
            return Ok(entries
                .iter()
                .filter_map(|entry| {
                    let config = &entry["package-http-server"]["config"];
                    if !config.is_object() {
                        return None;
                    }
                    let name = config["name"].as_str()?.to_string();
                    let properties = &config["package-http-properties"];
                    let links = &config["links"];
                    let port = properties["port"].as_u64().unwrap_or(0) as u16;
                    let content_database = links["database"]
                        .as_str()
                        .map(String::from)
                        .filter(|s| !s.is_empty());
                    let modules_database = links["modules-database"]
                        .as_str()
                        .map(String::from)
                        .filter(|s| !s.is_empty());
                    let secure = links["ssl-certificate-template"]
                        .as_str()
                        .is_some_and(|template| !template.is_empty())
                        || properties["ssl-enabled"].as_bool().unwrap_or(false);
                    let group = config["group-name"]
                        .as_str()
                        .filter(|group| !group.is_empty())
                        .unwrap_or("Default")
                        .to_string();
                    Some(AppServerEndpoint {
                        name,
                        port,
                        secure,
                        group,
                        content_database,
                        modules_database,
                        favorite: false,
                    })
                })
                .collect());
        }

        let entries = body
            .pointer("/server-default-list/list-items/list-item")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("unrecognized Manage API server list response"))?;
        Ok(entries
            .iter()
            .filter_map(|item| {
                let name = item["nameref"].as_str()?.to_string();
                let port = item["port"].as_u64().unwrap_or(0) as u16;
                let content_database = item["content-database"]
                    .as_str()
                    .map(String::from)
                    .filter(|s| !s.is_empty());
                let modules_database = item["modules-database"]
                    .as_str()
                    .map(String::from)
                    .filter(|s| !s.is_empty());
                let secure = item["ssl-certificate-template"]
                    .as_str()
                    .map(|s| !s.is_empty())
                    .or_else(|| item["ssl-enabled"].as_bool())
                    .unwrap_or(false);
                let group = item["group-name"]
                    .as_str()
                    .or_else(|| item["groupnameref"].as_str())
                    .or_else(|| item["group"].as_str())
                    .filter(|group| !group.is_empty())
                    .unwrap_or("Default")
                    .to_string();
                Some(AppServerEndpoint {
                    name,
                    port,
                    secure,
                    group,
                    content_database,
                    modules_database,
                    favorite: false,
                })
            })
            .collect())
    }

    async fn list_app_servers_through_eval(&self) -> Result<Vec<AppServerEndpoint>> {
        let auth_method = match self.server.auth_type {
            AuthType::Basic => "basic",
            _ => "digest",
        };
        let username = quick_xml::escape::escape(&self.server.username);
        let password = quick_xml::escape::escape(&self.server.password);
        let script = format!(
            r#"xquery version "1.0-ml";
            xdmp:http-get(
                "http://localhost:8002/manage/v2/servers?view=package&amp;format=json",
                <options xmlns="xdmp:http">
                    <authentication method="{auth_method}">
                        <username>{username}</username>
                        <password>{password}</password>
                    </authentication>
                </options>
            )[2]"#
        );
        let parts = self
            .xquery_query(&script)
            .await
            .context("Failed to discover app servers through the configured app server")?;
        for part in parts {
            if let Ok(body) = serde_json::from_str::<Value>(&part)
                && let Ok(servers) = Self::parse_app_servers(&body)
            {
                return Ok(servers);
            }
        }
        bail!("The configured app server returned an unrecognized server list")
    }

    /// List HTTP app servers with key properties from the manage API.
    pub async fn list_app_servers(&self) -> Result<Vec<AppServerEndpoint>> {
        let url = self
            .server
            .manage_url("/manage/v2/servers?view=package&format=json");
        let direct_result = async {
            let resp = self
                .request(Method::GET, &url, |r| r)
                .await
                .context("Failed to list app servers")?;
            let body: Value = resp.json().await?;
            Self::parse_app_servers(&body)
        }
        .await;
        let mut servers = match direct_result {
            Ok(servers) => servers,
            Err(manage_error) => self
                .list_app_servers_through_eval()
                .await
                .map_err(|fallback_error| {
                    anyhow::anyhow!(
                        "Management API discovery failed: {manage_error:#}; tunneled discovery failed: {fallback_error:#}"
                    )
                })?,
        };

        servers.sort_by(|left, right| {
            left.group
                .to_lowercase()
                .cmp(&right.group.to_lowercase())
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        Ok(servers)
    }

    /// List collections in current database
    pub async fn list_collections(&self) -> Result<Vec<String>> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let script = "JSON.stringify(Array.from(cts.collections()))";
        let url = self.system_url("/v1/eval");
        let body_str = format!("javascript={}", urlencoding::encode(script));
        let db_clone = db.clone();
        let resp = self
            .request(Method::POST, &url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", &db_clone)])
                    .body(body_str.clone())
            })
            .await
            .context("Failed to list collections")?;
        let text = resp.text().await?;
        let collections = parse_eval_json_array(&text);
        Ok(collections)
    }

    /// Get a document's raw content from a specific database.
    pub async fn get_document_content_from_database(
        &self,
        uri: &str,
        database: &str,
    ) -> Result<String> {
        let url = self.system_url("/v1/documents");
        let uri_owned = uri.to_string();
        let db_owned = database.to_string();
        let resp = self
            .request(Method::GET, &url, |r| {
                r.query(&[("database", db_owned.as_str()), ("uri", uri_owned.as_str())])
            })
            .await
            .with_context(|| format!("Failed to read '{}' from database '{}'", uri, database))?;
        Ok(resp.text().await?)
    }

    /// Search module URIs in a modules database by substring pattern, with paging.
    pub async fn search_module_uris(
        &self,
        modules_database: &str,
        pattern: &str,
        start: usize,
        page_size: usize,
    ) -> Result<ModuleSearchResult> {
        let url = self.system_url("/v1/eval");
        let script = build_module_search_script(pattern, start, page_size);
        let body_str = format!("javascript={}", urlencoding::encode(&script));
        let modules_db = modules_database.to_string();
        let resp = self
            .request(Method::POST, &url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", modules_db.as_str())])
                    .body(body_str.clone())
            })
            .await
            .with_context(|| {
                format!(
                    "Failed to search module URIs in modules database '{}'",
                    modules_database
                )
            })?;
        let text = resp.text().await?;
        let (uris, total) = parse_module_search_result(&text);
        Ok(ModuleSearchResult { uris, total })
    }

    /// Execute a JavaScript query via eval endpoint, returning individual result parts
    pub async fn js_query(&self, script: &str) -> Result<Vec<String>> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/eval");
        let body_str = format!("javascript={}", urlencoding::encode(script));
        let db_clone = db.clone();
        let resp = self
            .request(Method::POST, &url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", db_clone.as_str())])
                    .body(body_str.clone())
            })
            .await
            .context("Failed to execute JavaScript query")?;
        let text = resp.text().await?;
        parse_eval_response_parts(&text)
    }

    /// Execute an XQuery via eval endpoint, returning individual result parts
    pub async fn xquery_query(&self, script: &str) -> Result<Vec<String>> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/eval");
        let body_str = format!("xquery={}", urlencoding::encode(script));
        let db_clone = db.clone();
        let resp = self
            .request(Method::POST, &url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", db_clone.as_str())])
                    .body(body_str.clone())
            })
            .await
            .context("Failed to execute XQuery")?;
        let text = resp.text().await?;
        parse_eval_response_parts(&text)
    }

    /// Search/list documents in a collection with paging (lightweight, no full content)
    pub async fn search_documents(
        &self,
        collection: Option<&str>,
        query: Option<&str>,
        uri_filter: Option<&str>,
        start: usize,
        page_size: usize,
    ) -> Result<PagedResults> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();

        // If URI filter is set, use eval with cts.uriMatch for flexible matching
        if let Some(filter) = uri_filter {
            return self
                .search_documents_with_uri_filter(collection, filter, start, page_size, &db)
                .await;
        }

        let url = self.system_url("/v1/search");
        let start_str = start.to_string();
        let page_str = page_size.to_string();
        let collection_owned = collection.map(String::from);
        let query_owned = query.map(String::from);
        let db_clone = db.clone();

        let resp = self
            .request(Method::GET, &url, |mut r| {
                r = r.query(&[
                    ("database", db_clone.as_str()),
                    ("format", "json"),
                    ("start", start_str.as_str()),
                    ("pageLength", page_str.as_str()),
                ]);
                if let Some(ref col) = collection_owned {
                    r = r.query(&[("collection", col.as_str())]);
                }
                if let Some(ref q) = query_owned {
                    r = r.query(&[("q", q.as_str())]);
                }
                r
            })
            .await
            .context("Failed to search documents")?;
        let body: Value = resp.json().await?;

        let total = body["total"].as_u64().map(|t| t as usize);
        let mut results: Vec<SearchResult> = body["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|item| {
                        let uri = item["uri"].as_str().unwrap_or("").to_string();
                        // Collections aren't returned in default search; will be fetched on detail view
                        SearchResult {
                            uri,
                            collections: Vec::new(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Fetch collections for the returned URIs
        if !results.is_empty() {
            let uris_json =
                serde_json::to_string(&results.iter().map(|r| r.uri.as_str()).collect::<Vec<_>>())
                    .unwrap();
            let script = format!(
                r#"
                const uris = {};
                JSON.stringify(uris.map(uri => ({{
                    uri: uri,
                    collections: Array.from(xdmp.documentGetCollections(uri))
                }})))
                "#,
                uris_json
            );
            let eval_url = self.system_url("/v1/eval");
            let body_str = format!("javascript={}", urlencoding::encode(&script));
            let db_clone2 = db.clone();
            if let Ok(resp) = self
                .request(Method::POST, &eval_url, |r| {
                    r.header("Content-Type", "application/x-www-form-urlencoded")
                        .query(&[("database", db_clone2.as_str())])
                        .body(body_str.clone())
                })
                .await
            {
                if let Ok(text) = resp.text().await {
                    // Parse the multipart eval response
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with('[') {
                            if let Ok(arr) = serde_json::from_str::<Vec<Value>>(trimmed) {
                                for item in arr {
                                    let uri = item["uri"].as_str().unwrap_or("");
                                    let cols: Vec<String> = item["collections"]
                                        .as_array()
                                        .map(|a| {
                                            a.iter()
                                                .filter_map(|v| v.as_str().map(String::from))
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    if let Some(r) = results.iter_mut().find(|r| r.uri == uri) {
                                        r.collections = cols;
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }

        Ok(PagedResults { results, total })
    }

    /// Search documents with URI pattern filter using cts.uriMatch via eval
    async fn search_documents_with_uri_filter(
        &self,
        collection: Option<&str>,
        uri_filter: &str,
        start: usize,
        page_size: usize,
        db: &str,
    ) -> Result<PagedResults> {
        let col_filter = if let Some(col) = collection {
            format!(r#", cts.collectionQuery("{}")"#, col.replace('"', r#"\""#))
        } else {
            String::new()
        };
        let pattern = format!("*{}*", uri_filter.replace('"', r#"\""#));
        let script = format!(
            r#"'use strict';
            const uriArray = Array.from(cts.uriMatch("{pattern}"));
            const query = cts.andQuery([cts.documentQuery(uriArray){col_filter}]);
            const total = uriArray.length;
            const results = fn.subsequence(cts.search(query), {start}, {page_size});
            const output = [];
            for (const doc of results) {{
                const uri = xdmp.nodeUri(doc);
                const cols = Array.from(xdmp.documentGetCollections(uri));
                output.push({{uri: uri, collections: cols}});
            }}
            JSON.stringify({{total: total, results: output}});"#,
            pattern = pattern,
            col_filter = col_filter,
            start = start,
            page_size = page_size,
        );

        let eval_url = self.system_url("/v1/eval");
        let body_str = format!("javascript={}", urlencoding::encode(&script));
        let db_clone = db.to_string();

        let resp = self
            .request(Method::POST, &eval_url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", db_clone.as_str())])
                    .body(body_str.clone())
            })
            .await
            .context("Failed to search documents with URI filter")?;

        let text = resp.text().await?;
        let mut results = Vec::new();
        let mut total = None;

        let parts = parse_eval_response_parts(&text).unwrap_or_default();
        for part in &parts {
            let trimmed = part.trim();
            if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                total = val["total"].as_u64().map(|t| t as usize);
                if let Some(arr) = val["results"].as_array() {
                    for item in arr {
                        let uri = item["uri"].as_str().unwrap_or("").to_string();
                        let collections: Vec<String> = item["collections"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        results.push(SearchResult { uri, collections });
                    }
                }
                break;
            }
        }

        Ok(PagedResults { results, total })
    }

    /// Get a single document by URI with full content and metadata
    pub async fn get_document(&self, uri: &str) -> Result<DocumentDetail> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/documents");
        let uri_owned = uri.to_string();
        let db_clone = db.clone();

        // Get content
        let resp = self
            .request(Method::GET, &url, |r| {
                r.query(&[("database", db_clone.as_str()), ("uri", uri_owned.as_str())])
            })
            .await
            .context("Failed to get document")?;
        let content = resp.text().await?;

        let metadata = self.get_document_metadata(uri).await?;

        Ok(DocumentDetail {
            uri: uri.to_string(),
            collections: metadata.collections,
            quality: metadata.quality,
            permissions: metadata.permissions,
            content,
        })
    }

    /// Get a document's metadata without downloading its content.
    pub async fn get_document_metadata(&self, uri: &str) -> Result<DocumentMetadata> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/documents");
        let db_clone2 = db.clone();
        let uri_owned2 = uri.to_string();
        let meta_resp = self
            .request(Method::GET, &url, |r| {
                r.query(&[
                    ("database", db_clone2.as_str()),
                    ("uri", uri_owned2.as_str()),
                    ("category", "metadata"),
                    ("format", "json"),
                ])
            })
            .await
            .context("Failed to get document metadata")?;
        let meta_body: Value = meta_resp.json().await.unwrap_or(Value::Null);

        let collections = meta_body["collections"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let quality = meta_body["quality"].as_i64();
        let permissions = meta_body["permissions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let role = p["role-name"].as_str().unwrap_or("");
                        let caps = p["capabilities"]
                            .as_array()
                            .map(|c| {
                                c.iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            })
                            .unwrap_or_default();
                        if role.is_empty() {
                            None
                        } else {
                            Some(format!("{}:[{}]", role, caps))
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(DocumentMetadata {
            collections,
            quality,
            permissions,
        })
    }

    /// Count property nodes attached to a document.  A caller can treat a
    /// failure as unavailable metadata: access to document properties is
    /// commonly more restricted than access to the document itself.
    pub async fn document_property_count(&self, uri: &str) -> Result<usize> {
        let uri_literal = serde_json::to_string(uri)?;
        let script = format!("Array.from(xdmp.documentGetProperties({uri_literal})).length");
        let parts = self.js_query(&script).await?;
        let value = parts
            .first()
            .ok_or_else(|| anyhow::anyhow!("No property count returned for {uri}"))?
            .trim()
            .parse::<usize>()
            .context("Invalid document property count")?;
        Ok(value)
    }

    /// Replace a single document's content by URI, with optional collections and quality.
    pub async fn update_document(
        &self,
        uri: &str,
        content: &str,
        collections: Option<&[String]>,
        quality: Option<i64>,
    ) -> Result<()> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/documents");
        let uri_owned = uri.to_string();
        let db_clone = db.clone();
        let content_type = infer_document_content_type(uri, content);

        self.request(Method::PUT, &url, |r| {
            let mut builder = r
                .query(&[("database", db_clone.as_str()), ("uri", uri_owned.as_str())])
                .header("Content-Type", content_type)
                .body(content.to_string());
            if let Some(cols) = collections {
                if !cols.is_empty() {
                    builder = builder.header("X-ML-Document-Collections", cols.join(","));
                }
            }
            if let Some(q) = quality {
                builder = builder.header("X-ML-Document-Quality", q.to_string());
            }
            builder
        })
        .await
        .with_context(|| format!("Failed to update document: {}", uri))?;

        Ok(())
    }

    /// Replace a module's content in a specific modules database.
    pub async fn update_document_in_modules_database(
        &self,
        uri: &str,
        content: &str,
        modules_database: &str,
    ) -> Result<()> {
        let url = self.system_url("/v1/documents");
        let uri_owned = uri.to_string();
        let db_owned = modules_database.to_string();
        let content_type = infer_document_content_type(uri, content);

        self.request(Method::PUT, &url, |r| {
            r.query(&[("database", db_owned.as_str()), ("uri", uri_owned.as_str())])
                .header("Content-Type", content_type)
                .body(content.to_string())
        })
        .await
        .with_context(|| {
            format!(
                "Failed to update module '{}' in modules database '{}'",
                uri, modules_database
            )
        })?;

        Ok(())
    }

    /// Create a new document with URI, content, collections and quality.
    pub async fn create_document(
        &self,
        uri: &str,
        content: &str,
        collections: &[String],
        quality: i64,
    ) -> Result<()> {
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/documents");
        let uri_owned = uri.to_string();
        let db_clone = db.clone();
        let content_type = infer_document_content_type(uri, content);

        self.request(Method::PUT, &url, |r| {
            let mut builder = r
                .query(&[("database", db_clone.as_str()), ("uri", uri_owned.as_str())])
                .header("Content-Type", content_type)
                .body(content.to_string());
            if !collections.is_empty() {
                builder = builder.header("X-ML-Document-Collections", collections.join(","));
            }
            builder = builder.header("X-ML-Document-Quality", quality.to_string());
            builder
        })
        .await
        .with_context(|| format!("Failed to create document: {}", uri))?;

        Ok(())
    }

    /// Delete documents by URIs
    pub async fn delete_documents(&self, uris: &[String]) -> Result<()> {
        if uris.is_empty() {
            return Ok(());
        }
        let db = self.database.as_deref().unwrap_or("Documents").to_string();
        let url = self.system_url("/v1/documents");

        // Delete each URI individually via DELETE /v1/documents?uri=...
        for uri in uris {
            let db_clone = db.clone();
            let uri_clone = uri.clone();
            self.request(Method::DELETE, &url, |r| {
                r.query(&[("database", db_clone.as_str()), ("uri", uri_clone.as_str())])
            })
            .await
            .with_context(|| format!("Failed to delete document: {}", uri))?;
        }
        Ok(())
    }
}

/// Parse eval multipart response to extract JSON array
fn parse_eval_json_array(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') || trimmed.starts_with('"') {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(trimmed) {
                return arr;
            }
            if let Ok(s) = serde_json::from_str::<String>(trimmed) {
                results.push(s);
            }
        }
    }
    results
}

/// Parse multipart/mixed eval response into individual result parts
fn parse_eval_response_parts(text: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut in_content = false;
    let mut current_content = String::new();

    for line in text.lines() {
        if line.starts_with("--") && !line.starts_with("---") {
            if in_content && !current_content.trim().is_empty() {
                parts.push(current_content.trim().to_string());
            }
            current_content.clear();
            in_content = false;
        } else if line.is_empty() && !in_content {
            in_content = true;
        } else if in_content {
            if !current_content.is_empty() {
                current_content.push('\n');
            }
            current_content.push_str(line);
        }
    }
    if in_content && !current_content.trim().is_empty() {
        parts.push(current_content.trim().to_string());
    }

    // Pretty-print JSON parts
    let formatted: Vec<String> = parts
        .iter()
        .map(|p| {
            if let Ok(val) = serde_json::from_str::<Value>(p) {
                serde_json::to_string_pretty(&val).unwrap_or_else(|_| p.clone())
            } else {
                p.clone()
            }
        })
        .collect();

    Ok(formatted)
}

fn infer_document_content_type(uri: &str, content: &str) -> &'static str {
    let trimmed = content.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return "application/json";
    }
    if trimmed.starts_with('<') {
        return "application/xml";
    }

    match Path::new(uri)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("json") => "application/json",
        Some("xml" | "xhtml" | "svg") => "application/xml",
        _ => "text/plain",
    }
}

fn escape_uri_match_pattern(pattern: &str) -> String {
    pattern
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('?', "\\?")
}

/// Build the eval script for module URI search. Paging uses `Array.slice`
/// on the materialized JS array: `fn.subsequence` returns a MarkLogic
/// Sequence which `JSON.stringify` renders as a string (or nested array),
/// not a flat array. `start` is 1-based; slice is 0-based.
fn build_module_search_script(pattern: &str, start: usize, page_size: usize) -> String {
    let match_pattern = module_uri_match_pattern(pattern);
    let safe_pattern = match_pattern.replace('"', "\\\"");
    let zero_based = start.saturating_sub(1);
    format!(
        r#"
            'use strict';
            const uriArray = Array.from(cts.uriMatch("{}"));
            const total = uriArray.length;
            const results = uriArray.slice({}, {} + {});
            JSON.stringify({{total: total, results: results}});
            "#,
        safe_pattern, zero_based, zero_based, page_size
    )
}

/// Build the `cts.uriMatch` pattern from raw user input:
/// - empty input matches everything (`*`);
/// - input containing `*` or `?` is honored as a wildcard pattern
///   (only backslashes are escaped so user wildcards survive);
/// - anything else becomes a `*...*` substring match.
fn module_uri_match_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return "*".to_string();
    }
    if trimmed.contains('*') || trimmed.contains('?') {
        trimmed.replace('\\', "\\\\")
    } else {
        format!("*{}*", escape_uri_match_pattern(trimmed))
    }
}

/// Extract `(uris, total)` from a `/v1/eval` response body.
/// Falls back to parsing the whole body when it is not multipart
/// (single-part responses would otherwise look like "no matches").
fn parse_module_search_result(text: &str) -> (Vec<String>, Option<usize>) {
    let parts = parse_eval_response_parts(text).unwrap_or_default();
    let owned_fallback;
    let candidates: &[String] = if parts.is_empty() {
        owned_fallback = vec![text.trim().to_string()];
        &owned_fallback
    } else {
        &parts
    };
    let mut uris = Vec::new();
    let mut total = None;
    for part in candidates {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            total = val["total"].as_u64().map(|t| t as usize);
            uris = extract_result_uris(&val["results"]);
            break;
        }
    }
    (uris, total)
}

/// Extract URIs from the `results` field of a module search response.
/// Normally a JSON array, but `fn.subsequence` output stringifies to a
/// JSON string containing an array, so parse that shape too.
fn extract_result_uris(results: &Value) -> Vec<String> {
    if let Some(arr) = results.as_array() {
        return arr
            .iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect();
    }
    if let Some(inner) = results.as_str() {
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(inner) {
            return arr
                .iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_client() -> MarkLogicClient {
        let server = ServerConfig {
            name: "local".to_string(),
            host: "localhost".to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8000,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: vec![],
            uri: None,
        };
        MarkLogicClient::new(server).expect("Failed to create test client")
    }

    #[test]
    fn replacing_download_counter_detaches_previous_operation_progress() {
        let old_progress = Arc::new(AtomicU64::new(0));
        let counter = Arc::new(AtomicU64::new(0));
        let client = test_client()
            .with_download_progress(old_progress.clone())
            .with_download_counter(counter.clone());
        client.count_download(42);
        assert_eq!(old_progress.load(Ordering::Relaxed), 0);
        assert_eq!(counter.load(Ordering::Relaxed), 42);
    }

    #[tokio::test]
    async fn download_progress_updates_before_response_finishes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc")
                .await
                .unwrap();
            wait.await.unwrap();
            stream.write_all(b"def").await.unwrap();
        });
        let counter = Arc::new(AtomicU64::new(0));
        let progress = Arc::new(AtomicU64::new(0));
        let client = test_client()
            .with_download_counter(counter.clone())
            .with_download_progress(progress.clone());
        let response = client
            .client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        let reader =
            tokio::spawn(async move { client.read_response_body(response).await.unwrap() });
        timeout(Duration::from_secs(2), async {
            while progress.load(Ordering::Relaxed) < 3 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("first response chunk should update progress while download is pending");
        assert_eq!(progress.load(Ordering::Relaxed), 3);
        assert!(!reader.is_finished());
        release.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), reader)
                .await
                .unwrap()
                .unwrap(),
            b"abcdef"
        );
        assert_eq!(progress.load(Ordering::Relaxed), 6);
        assert_eq!(counter.load(Ordering::Relaxed), 6);
        server.await.unwrap();
    }

    #[test]
    fn parse_app_servers_reads_manage_package_view() {
        let body = serde_json::json!({
            "set": {
                "servers": [
                    {
                        "package-http-server": {
                            "config": {
                                "name": "App-Services",
                                "group-name": "Default",
                                "package-http-properties": { "port": 8000 },
                                "links": {
                                    "database": "Documents",
                                    "modules-database": "Modules"
                                }
                            }
                        }
                    },
                    {
                        "package-http-server": {
                            "config": {
                                "name": "Secure-Services",
                                "group-name": "Group2",
                                "package-http-properties": { "port": 8043 },
                                "links": {
                                    "database": "Secure-Content",
                                    "modules-database": "Secure-Modules",
                                    "ssl-certificate-template": "internal"
                                }
                            }
                        }
                    },
                    {
                        "package-xdbc-server": {
                            "config": {
                                "name": "XDBC",
                                "group-name": "Default"
                            }
                        }
                    }
                ]
            }
        });

        let servers = MarkLogicClient::parse_app_servers(&body).unwrap();

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "App-Services");
        assert_eq!(servers[0].port, 8000);
        assert_eq!(servers[0].group, "Default");
        assert!(!servers[0].secure);
        assert_eq!(servers[0].content_database.as_deref(), Some("Documents"));
        assert_eq!(servers[0].modules_database.as_deref(), Some("Modules"));
        assert_eq!(servers[1].name, "Secure-Services");
        assert_eq!(servers[1].port, 8043);
        assert_eq!(servers[1].group, "Group2");
        assert!(servers[1].secure);
        assert_eq!(
            servers[1].content_database.as_deref(),
            Some("Secure-Content")
        );
        assert_eq!(
            servers[1].modules_database.as_deref(),
            Some("Secure-Modules")
        );
    }

    #[test]
    fn parse_app_servers_supports_default_list_and_rejects_unknown_responses() {
        let body = serde_json::json!({
            "server-default-list": {
                "list-items": {
                    "list-item": [{
                        "nameref": "App-Services",
                        "groupnameref": "Default",
                        "port": 8000,
                        "content-database": "Documents",
                        "modules-database": "Modules"
                    }]
                }
            }
        });

        let servers = MarkLogicClient::parse_app_servers(&body).unwrap();

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "App-Services");
        assert_eq!(servers[0].group, "Default");
        assert_eq!(servers[0].content_database.as_deref(), Some("Documents"));
        assert!(MarkLogicClient::parse_app_servers(&serde_json::json!({})).is_err());
    }

    #[tokio::test]
    async fn test_list_databases() {
        let client = test_client();
        let result = client.list_databases().await;
        match result {
            Ok(dbs) => {
                println!("Databases: {:?}", dbs);
                assert!(!dbs.is_empty(), "Should have at least one database");
                // MarkLogic always has these system databases
                assert!(
                    dbs.iter()
                        .any(|d| d == "Documents" || d == "Security" || d == "Schemas"),
                    "Should contain system databases, got: {:?}",
                    dbs
                );
            }
            Err(e) => {
                panic!("Failed to list databases: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_list_collections() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let result = client.list_collections().await;
        match result {
            Ok(cols) => {
                println!("Collections: {:?}", cols);
                assert!(
                    !cols.is_empty(),
                    "Should have collections in data-platform-content"
                );
            }
            Err(e) => {
                panic!("Failed to list collections: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_search_documents() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let result = client.search_documents(None, None, None, 1, 10).await;
        match result {
            Ok(paged) => {
                println!(
                    "Search results: {} docs, total: {:?}",
                    paged.results.len(),
                    paged.total
                );
                for r in &paged.results {
                    println!("  URI: {} | Collections: {:?}", r.uri, r.collections);
                }
                // At least some should have collections
                let has_collections = paged.results.iter().any(|r| !r.collections.is_empty());
                println!("Has collections: {}", has_collections);
            }
            Err(e) => {
                panic!("Failed to search documents: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_get_collections_for_uri() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        // First get a URI
        let paged = client
            .search_documents(None, None, None, 1, 1)
            .await
            .unwrap();
        let uri = &paged.results[0].uri;
        println!("Testing collections for URI: {}", uri);

        let db = "data-platform-content";
        let script = format!(
            r#"JSON.stringify(Array.from(xdmp.documentGetCollections("{}")))"#,
            uri
        );
        let eval_url = client.system_url("/v1/eval");
        let body_str = format!("javascript={}", script);
        let resp = client
            .request(Method::POST, &eval_url, |r| {
                r.header("Content-Type", "application/x-www-form-urlencoded")
                    .query(&[("database", db)])
                    .body(body_str.clone())
            })
            .await
            .unwrap();
        let text = resp.text().await.unwrap();
        println!("Eval response for collections:\n{}", text);
    }

    #[tokio::test]
    async fn test_search_raw_response() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let db = "data-platform-content";
        let url = client.system_url("/v1/search");
        let resp = client
            .request(Method::GET, &url, |r| {
                r.query(&[
                    ("database", db),
                    ("format", "json"),
                    ("start", "1"),
                    ("pageLength", "5"),
                ])
            })
            .await
            .unwrap();
        let text = resp.text().await.unwrap();
        println!("Raw search response:\n{}", &text[..text.len().min(2000)]);
    }

    #[tokio::test]
    async fn test_js_query() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let script = r#"
            'use strict';
            fn.subsequence(fn.doc(), 1, 5);
        "#;
        let result = client.js_query(script).await;
        match result {
            Ok(output) => {
                println!("JS query result:\n{:?}", &output[..output.len().min(1000)]);
                assert!(!output.is_empty(), "Should return some output");
            }
            Err(e) => {
                panic!("Failed to execute JS query: {}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_search_with_uri_filter() {
        let mut client = test_client();
        client.set_database("data-platform-content".to_string());
        let result = client
            .search_documents(None, None, Some("/activity/"), 1, 5)
            .await;
        match result {
            Ok(paged) => {
                println!(
                    "Filtered results: {} docs, total: {:?}",
                    paged.results.len(),
                    paged.total
                );
                for r in &paged.results {
                    println!("  URI: {} | Collections: {:?}", r.uri, r.collections);
                    assert!(
                        r.uri.contains("/activity/"),
                        "URI should contain /activity/"
                    );
                }
                assert!(
                    !paged.results.is_empty(),
                    "Should find documents matching /activity/"
                );
            }
            Err(e) => {
                panic!("Failed to search with URI filter: {}", e);
            }
        }
    }

    #[test]
    fn module_uri_match_pattern_honors_user_wildcards() {
        assert_eq!(module_uri_match_pattern(""), "*");
        assert_eq!(module_uri_match_pattern("   "), "*");
        assert_eq!(module_uri_match_pattern("foo"), "*foo*");
        // User-supplied wildcards must survive (previously escaped to literals).
        assert_eq!(module_uri_match_pattern("*"), "*");
        assert_eq!(module_uri_match_pattern("*.xqy"), "*.xqy");
        assert_eq!(module_uri_match_pattern("/foo/*"), "/foo/*");
        assert_eq!(module_uri_match_pattern("a?b"), "a?b");
        assert_eq!(module_uri_match_pattern("  bar  "), "*bar*");
    }

    #[test]
    fn module_search_script_returns_a_real_array() {
        // Paging must use slice on the materialized array: fn.subsequence
        // yields a Sequence that JSON.stringify renders as a string.
        let script = build_module_search_script("get", 1, 50);
        assert!(script.contains("cts.uriMatch(\"*get*\")"), "{}", script);
        assert!(script.contains("uriArray.slice(0, 0 + 50)"), "{}", script);
        assert!(!script.contains("fn.subsequence"), "{}", script);
        let script = build_module_search_script("*.xqy", 51, 50);
        assert!(script.contains("cts.uriMatch(\"*.xqy\")"), "{}", script);
        assert!(script.contains("uriArray.slice(50, 50 + 50)"), "{}", script);
    }

    #[test]
    fn parse_module_search_result_handles_multipart_and_plain_bodies() {
        let multipart = "--boundary\r\nContent-Type: application/json\r\n\r\n\
            {\"total\": 2, \"results\": [\"/a.xqy\", \"/b.xqy\"]}\r\n\
            --boundary--\r\n";
        let (uris, total) = parse_module_search_result(multipart);
        assert_eq!(total, Some(2));
        assert_eq!(uris, vec!["/a.xqy".to_string(), "/b.xqy".to_string()]);

        // Single-part (non-multipart) bodies must not report "no matches".
        let plain = r#"{"total": 1, "results": ["/c.xqy"]}"#;
        let (uris, total) = parse_module_search_result(plain);
        assert_eq!(total, Some(1));
        assert_eq!(uris, vec!["/c.xqy".to_string()]);

        // fn.subsequence output stringifies `results` as a JSON string
        // containing an array (seen live against MarkLogic): still parse it.
        let stringified = r#"{"total": 2, "results": "[\"/a.xqy\", \"/b.xqy\"]"}"#;
        let (uris, total) = parse_module_search_result(stringified);
        assert_eq!(total, Some(2));
        assert_eq!(uris, vec!["/a.xqy".to_string(), "/b.xqy".to_string()]);
    }

    #[test]
    fn infer_document_content_type_uses_content_before_extension() {
        assert_eq!(
            infer_document_content_type("/example.txt", r#"{"ok":true}"#),
            "application/json"
        );
        assert_eq!(
            infer_document_content_type("/example.txt", "<root/>"),
            "application/xml"
        );
    }

    #[test]
    fn infer_document_content_type_falls_back_to_uri_extension() {
        assert_eq!(
            infer_document_content_type("/example.json", "not-json"),
            "application/json"
        );
        assert_eq!(
            infer_document_content_type("/example.xml", "not-xml"),
            "application/xml"
        );
        assert_eq!(
            infer_document_content_type("/example.txt", "plain text"),
            "text/plain"
        );
        assert_eq!(
            Path::new("/example.txt")
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("txt")
        );
    }
}

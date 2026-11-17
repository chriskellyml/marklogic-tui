use crate::workspace::{Connection, server_for};
use marklogic_core::{
    client::{AuthType, MarkLogicClient, ServerConfig},
    config::AppConfig,
};
use reqwest::{
    Method,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RESPONSE_LIMIT: usize = 10 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestCredentials {
    username: String,
    password: String,
    auth_type: Option<AuthType>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurlExport {
    path: String,
    script: String,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn curl_script(
    connection: Connection,
    request: RestRequest,
    credentials: Option<RestCredentials>,
) -> anyhow::Result<String> {
    let method = Method::from_bytes(request.method.as_bytes())?;
    anyhow::ensure!(
        [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS
        ]
        .contains(&method),
        "Unsupported HTTP method"
    );
    anyhow::ensure!(
        request.path.starts_with('/')
            && !request.path.starts_with("//")
            && !request.path.contains(['\\', '#']),
        "Use a relative request path beginning with /, without a fragment"
    );
    anyhow::ensure!(
        request
            .body
            .as_ref()
            .is_none_or(|body| body.len() <= 5 * 1024 * 1024),
        "Request body exceeds 5 MiB"
    );

    let mut server = server_for(&AppConfig::load()?, &connection)?;
    let has_auth_header = request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    anyhow::ensure!(
        !has_auth_header || credentials.is_none(),
        "Remove the Authorization header to use the REST user override"
    );
    if let Some(credentials) = credentials {
        apply_credentials(&mut server, credentials)?;
    }

    let url = reqwest::Url::parse(&format!("{}{}", server.base_url(), request.path))?;
    anyhow::ensure!(
        url.origin() == reqwest::Url::parse(&server.base_url())?.origin(),
        "Request must use the selected environment"
    );
    let mut args = vec![
        "--silent".to_string(),
        "--show-error".to_string(),
        "--request".to_string(),
        shell_quote(method.as_str()),
        "--url".to_string(),
        shell_quote(url.as_str()),
    ];
    if server.insecure {
        args.push("--insecure".into());
    }
    for (name, value) in request.headers {
        let name = HeaderName::from_bytes(name.as_bytes())?;
        anyhow::ensure!(
            name != reqwest::header::HOST && name != reqwest::header::CONTENT_LENGTH,
            "Host and Content-Length are set automatically"
        );
        HeaderValue::from_str(&value)?;
        args.push("--header".into());
        args.push(shell_quote(&format!("{name}: {value}")));
    }
    if let Some(body) = request.body {
        args.push("--data-binary".into());
        args.push(shell_quote(&body));
    }
    match server.auth_type {
        AuthType::Digest => args.push("--digest".into()),
        AuthType::Basic => args.push("--basic".into()),
        AuthType::DigestBasic => args.push("--anyauth".into()),
        AuthType::ApplicationLevel => {}
    }
    if server.auth_type != AuthType::ApplicationLevel {
        args.push("--user".into());
        args.push(shell_quote(&format!(
            "{}:{}",
            server.username, server.password
        )));
    }
    Ok(format!(
        "#!/bin/sh\nset -eu\n\ncurl \\\n  {}\n",
        args.join(" \\\n  ")
    ))
}

#[tauri::command]
pub fn export_rest_curl(
    connection: Connection,
    request: RestRequest,
    credentials: Option<RestCredentials>,
) -> Result<CurlExport, String> {
    let script =
        curl_script(connection, request, credentials).map_err(|error| format!("{error:#}"))?;
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    for offset in 0..100u128 {
        let path = PathBuf::from(format!("/tmp/marklogic-tui-{}.sh", base + offset));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(script.as_bytes())
                    .map_err(|error| format!("Could not write cURL script: {error}"))?;
                return Ok(CurlExport {
                    path: path.to_string_lossy().into_owned(),
                    script,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Could not create cURL script: {error}")),
        }
    }
    Err("Could not find a unique temporary filename for the cURL script".into())
}

#[tauri::command]
pub fn open_rest_curl(path: String) -> Result<(), String> {
    if !path.starts_with("/tmp/marklogic-tui-") || !path.ends_with(".sh") {
        return Err("Invalid cURL script path".into());
    }
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&path).spawn()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("xdg-open").arg(&path).spawn()
    } else {
        return Err("Opening exported scripts is not supported on this platform".into());
    };
    result
        .map(|_| ())
        .map_err(|error| format!("Could not open cURL script: {error}"))
}

fn apply_credentials(
    server: &mut ServerConfig,
    credentials: RestCredentials,
) -> anyhow::Result<()> {
    anyhow::ensure!(!credentials.username.trim().is_empty(), "Enter a username");
    if server.auth_type == AuthType::ApplicationLevel {
        server.auth_type = credentials
            .auth_type
            .filter(|auth| *auth != AuthType::ApplicationLevel)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Choose Basic, Digest, or Digest / Basic authentication for the override"
                )
            })?;
    }
    server.username = credentials.username;
    server.password = credentials.password;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestResponse {
    url: String,
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: String,
    elapsed_ms: u128,
    size: usize,
    truncated: bool,
}

#[tauri::command]
pub async fn execute_rest_request(
    connection: Connection,
    request: RestRequest,
    credentials: Option<RestCredentials>,
) -> Result<RestResponse, String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        execute(connection, request, credentials),
    )
    .await
    .map_err(|_| {
        "REST request timed out after 60 seconds. Check the server before retrying a write."
            .to_string()
    })?
    .map_err(|error| format!("{error:#}"))
}

async fn execute(
    connection: Connection,
    request: RestRequest,
    credentials: Option<RestCredentials>,
) -> anyhow::Result<RestResponse> {
    let method = Method::from_bytes(request.method.as_bytes())?;
    anyhow::ensure!(
        [
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS
        ]
        .contains(&method),
        "Unsupported HTTP method"
    );
    anyhow::ensure!(
        request
            .body
            .as_ref()
            .is_none_or(|body| body.len() <= 5 * 1024 * 1024),
        "Request body exceeds 5 MiB"
    );
    let mut headers = HeaderMap::new();
    for (name, value) in request.headers {
        let name = HeaderName::from_bytes(name.as_bytes())?;
        anyhow::ensure!(
            name != reqwest::header::HOST && name != reqwest::header::CONTENT_LENGTH,
            "Host and Content-Length are set automatically"
        );
        anyhow::ensure!(
            credentials.is_none() || name != reqwest::header::AUTHORIZATION,
            "Remove the Authorization header to use the REST user override"
        );
        headers.append(name, HeaderValue::from_str(&value)?);
    }
    let overridden = credentials.is_some();
    let mut server = server_for(&AppConfig::load()?, &connection)?;
    if let Some(credentials) = credentials {
        apply_credentials(&mut server, credentials)?;
    }
    let client = MarkLogicClient::new(server)?;
    let client = if overridden {
        client.without_curl_debug()
    } else {
        client
    };
    let started = Instant::now();
    let mut response = client
        .rest_request(method, &request.path, headers, request.body)
        .await?;
    let status = response.status();
    let url = response.url().to_string();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut bytes = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        let remaining = RESPONSE_LIMIT - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            truncated = true;
            break;
        }
    }
    Ok(RestResponse {
        url,
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        elapsed_ms: started.elapsed().as_millis(),
        size: bytes.len(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(auth_type: AuthType) -> ServerConfig {
        ServerConfig {
            name: "local".into(),
            host: "localhost".into(),
            username: "saved".into(),
            password: "saved-password".into(),
            port: 8000,
            secure: false,
            auth_type,
            insecure: false,
            app_servers: vec![],
            color: None,
            stripe_color_secondary: None,
            uri: None,
        }
    }

    #[test]
    fn override_changes_only_the_request_copy_and_retains_the_profile_auth_method() {
        for auth_type in [AuthType::Basic, AuthType::Digest, AuthType::DigestBasic] {
            let original = server(auth_type);
            let mut request_server = original.clone();
            apply_credentials(
                &mut request_server,
                RestCredentials {
                    username: "analyst".into(),
                    password: "temporary".into(),
                    auth_type: Some(AuthType::Basic),
                },
            )
            .unwrap();
            assert_eq!(request_server.username, "analyst");
            assert_eq!(request_server.password, "temporary");
            assert_eq!(request_server.auth_type, auth_type);
            assert_eq!(original.username, "saved");
            assert_eq!(original.password, "saved-password");
        }
    }

    #[test]
    fn application_level_override_requires_an_explicit_http_auth_method() {
        let mut request_server = server(AuthType::ApplicationLevel);
        assert!(
            apply_credentials(
                &mut request_server,
                RestCredentials {
                    username: "analyst".into(),
                    password: "temporary".into(),
                    auth_type: None
                }
            )
            .is_err()
        );
        assert!(
            apply_credentials(
                &mut request_server,
                RestCredentials {
                    username: " ".into(),
                    password: "temporary".into(),
                    auth_type: Some(AuthType::Digest)
                }
            )
            .is_err()
        );
        apply_credentials(
            &mut request_server,
            RestCredentials {
                username: "analyst".into(),
                password: "temporary".into(),
                auth_type: Some(AuthType::DigestBasic),
            },
        )
        .unwrap();
        assert_eq!(request_server.auth_type, AuthType::DigestBasic);
    }
}

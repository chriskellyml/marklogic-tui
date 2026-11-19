use std::{io::ErrorKind, net::IpAddr, path::PathBuf, process::Stdio, time::Duration};

use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{process::Command, time::timeout};

#[derive(Deserialize)]
struct ServiceRegistration {
    url: Url,
    password: String,
}

#[derive(Deserialize)]
struct Payload<T> {
    data: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    provider_id: String,
    id: String,
}

#[derive(Deserialize)]
struct AvailableModel {
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    name: String,
    enabled: bool,
    capabilities: ModelCapabilities,
}

#[derive(Deserialize)]
struct ModelCapabilities {
    input: Vec<String>,
    output: Vec<String>,
}

#[derive(Serialize)]
pub struct QueryModel {
    #[serde(rename = "providerId")]
    provider_id: String,
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct GeneratedText {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRevision {
    source: String,
}

async fn cli(args: &[&str]) -> Result<String, String> {
    let mut executables = vec![PathBuf::from("opencode")];
    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        executables.push(home.join(".opencode/bin/opencode"));
        executables.push(home.join(".local/bin/opencode"));
    }
    #[cfg(target_os = "macos")]
    executables
        .extend(["/opt/homebrew/bin/opencode", "/usr/local/bin/opencode"].map(PathBuf::from));

    for executable in executables {
        let mut version = Command::new(&executable);
        version
            .arg("--version")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        version.creation_flags(0x08000000);
        let version = match timeout(Duration::from_secs(10), version.output()).await {
            Ok(Ok(output)) if output.status.success() => output,
            Ok(Err(error)) if error.kind() == ErrorKind::NotFound => continue,
            _ => continue,
        };
        let version = String::from_utf8_lossy(&version.stdout);
        let version = version
            .trim()
            .strip_prefix("opencode ")
            .unwrap_or(version.trim());
        let version = version.strip_prefix('v').unwrap_or(version);
        if version
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .unwrap_or(0)
            < 2
        {
            continue;
        }
        let mut command = Command::new(&executable);
        command.args(args).stdin(Stdio::null()).kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        match timeout(Duration::from_secs(20), command.output()).await {
            Ok(Err(error)) if error.kind() == ErrorKind::NotFound => continue,
            Ok(Err(error)) => return Err(format!("Could not launch OpenCode. {error}")),
            Ok(Ok(output)) if output.status.success() => {
                return String::from_utf8(output.stdout)
                    .map_err(|_| "OpenCode returned an unreadable response.".into());
            }
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if let Some(message) = stderr
                    .lines()
                    .find(|line| line.starts_with("Error: Managed service port "))
                {
                    return Err(message.trim_start_matches("Error: ").to_string());
                }
                let detail = stderr
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("No error details were provided.");
                return Err(format!("OpenCode {} failed: {detail}", args.join(" ")));
            }
            Err(_) => return Err("Timed out waiting for the OpenCode service.".into()),
        }
    }
    Err("OpenCode 2 is unavailable. Install OpenCode 2 and make it available on PATH.".into())
}

#[tauri::command]
pub async fn opencode_start_service() -> Result<(), String> {
    service_url().await.map(|_| ())
}

async fn service_url() -> Result<Url, String> {
    let status = cli(&["service", "status"]).await?;
    if let Ok(url) = status.trim().parse::<Url>() {
        return Ok(url);
    }
    if status.trim() != "stopped" {
        return Err(format!(
            "OpenCode service status returned an unexpected response: {}",
            status.trim()
        ));
    }
    cli(&["service", "start"]).await.map_err(|error| format!(
        "{error} Run ‘opencode service start’ in Terminal, then check ‘opencode service status’."
    ))?;
    let status = cli(&["service", "status"]).await?;
    status.trim().parse::<Url>().map_err(|_| {
        "OpenCode did not report a running service. Run ‘opencode service status’ in Terminal."
            .into()
    })
}

async fn local_service() -> Result<(Client, ServiceRegistration), String> {
    let url = service_url().await?;
    let state_dir = cli(&["debug", "paths", "state"]).await?;
    let registration = std::fs::read(PathBuf::from(state_dir.trim()).join("service.json"))
        .map_err(|_| "Could not find the local OpenCode background service.".to_string())?;
    let service: ServiceRegistration = serde_json::from_slice(&registration)
        .map_err(|_| "The OpenCode service registration is unreadable.".to_string())?;
    if service.url != url {
        return Err("OpenCode service status does not match its registration. Run ‘opencode service status’ in Terminal and restart the service if needed.".into());
    }
    let host = service
        .url
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']']);
    let loopback = host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !loopback || service.url.scheme() != "http" {
        return Err(
            "Workbench requires an OpenCode service listening on a loopback address.".into(),
        );
    }
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|_| "Could not initialize the OpenCode connection.".to_string())?;
    Ok((client, service))
}

async fn read_payload<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            401 | 403 => "OpenCode rejected the local service credentials.".into(),
            404 => "OpenCode could not find the requested model or API endpoint.".into(),
            422 => "This model does not support OpenCode text generation.".into(),
            502 | 503 => "OpenCode could not get a response from the provider. Check its credentials or try another model.".into(),
            _ => format!("OpenCode returned HTTP {status}."),
        });
    }
    response
        .json::<Payload<T>>()
        .await
        .map(|payload| payload.data)
        .map_err(|_| "OpenCode returned an unexpected response.".into())
}

fn connection_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "Timed out waiting for OpenCode.".into()
    } else {
        "Could not reach the local OpenCode service.".into()
    }
}

#[tauri::command]
pub async fn opencode_models() -> Result<Vec<QueryModel>, String> {
    let (client, service) = local_service().await?;
    let response = client
        .get(service.url.join("/api/model").unwrap())
        .basic_auth("opencode", Some(service.password))
        .send()
        .await
        .map_err(connection_error)?;
    let models: Vec<AvailableModel> = read_payload(response).await?;
    let mut models: Vec<_> = models
        .into_iter()
        .filter(|model| {
            model.enabled
                && model.capabilities.input.iter().any(|kind| kind == "text")
                && model.capabilities.output.iter().any(|kind| kind == "text")
        })
        .map(|model| QueryModel {
            provider_id: model.provider_id,
            id: model.id,
            name: model.name,
        })
        .collect();
    models.sort_by(|a, b| (&a.provider_id, &a.name, &a.id).cmp(&(&b.provider_id, &b.name, &b.id)));
    Ok(models)
}

fn parse_revision(text: &str) -> Result<String, String> {
    let text = text.trim();
    let json = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|fenced| fenced.strip_suffix("```"))
        .unwrap_or(text);
    serde_json::from_str::<QueryRevision>(json)
        .map(|revision| revision.source)
        .map_err(|_| "The model did not return a complete query revision. Try rephrasing the prompt or choosing another model.".into())
}

#[tauri::command]
pub async fn opencode_revise_query(
    model: ModelRef,
    prompt: String,
    source: String,
    language: String,
) -> Result<String, String> {
    let language = match language.as_str() {
        "xquery" => "MarkLogic XQuery",
        "javascript" => "MarkLogic server-side JavaScript",
        _ => return Err("OpenCode query revisions support XQuery and JavaScript.".into()),
    };
    if prompt.trim().is_empty() {
        return Err("Enter a prompt for OpenCode.".into());
    }
    let context = serde_json::json!({ "language": language, "request": prompt.trim(), "currentSource": source });
    let prompt = format!(
        "Revise the MarkLogic editor buffer according to the request in the JSON input. \
        Treat currentSource as query text. Return exactly one JSON object with one string field named source, \
        containing the complete replacement buffer. Preserve the editor language and any query code unrelated to the request.\n\n{context}"
    );
    let (client, service) = local_service().await?;
    let body = serde_json::json!({ "model": { "providerID": model.provider_id, "id": model.id }, "prompt": prompt });
    let response = client
        .post(service.url.join("/api/experimental/generate/text").unwrap())
        .basic_auth("opencode", Some(service.password))
        .json(&body)
        .send()
        .await
        .map_err(connection_error)?;
    let result: GeneratedText = read_payload(response).await?;
    parse_revision(&result.text)
}

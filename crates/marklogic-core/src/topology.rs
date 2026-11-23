//! Read-only cluster topology, shared independently of either UI.
use crate::client::MarkLogicClient;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DISCOVER: &str = include_str!("topology.xqy");

// MarkLogic identifiers are unsigned 64-bit integers. Keep them as strings all
// the way to the UI, where JavaScript numbers would silently lose precision.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub captured_at: String,
    pub groups: Vec<Group>,
    pub hosts: Vec<Host>,
    pub app_servers: Vec<AppServer>,
    pub databases: Vec<Database>,
    pub forests: Vec<Forest>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Group {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    pub id: String,
    pub name: String,
    pub group_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppServer {
    pub id: String,
    pub name: String,
    pub group_id: String,
    pub host_ids: Vec<String>,
    pub kind: String,
    pub port: u16,
    pub secure: bool,
    pub enabled: bool,
    pub content_database_id: String,
    pub modules_database_id: String,
    pub root: String,
    pub authentication: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Database {
    pub id: String,
    pub name: String,
    pub forest_ids: Vec<String>,
    pub security_database_id: String,
    pub schema_database_id: String,
    pub triggers_database_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Forest {
    pub id: String,
    pub name: String,
    pub host_id: String,
    pub replica_ids: Vec<String>,
    pub failover_host_ids: Vec<String>,
    pub data_directory: String,
    pub enabled: bool,
}

pub async fn discover(client: &MarkLogicClient) -> Result<Snapshot> {
    let value = client.configuration_query(DISCOVER).await
        .context("Unable to read cluster configuration. The connection needs eval and Admin API read privileges")?;
    serde_json::from_value(value).context("Invalid topology configuration response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{AuthType, ServerConfig};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn response_client(status: &str, body: &str) -> MarkLogicClient {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            let headers = String::from_utf8_lossy(&header);
            assert!(headers.starts_with("POST /v1/eval"));
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            let query = String::from_utf8(body).unwrap();
            assert!(query.contains("admin%3Aget-configuration"));
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        MarkLogicClient::new(ServerConfig {
            name: "test".into(),
            host: "127.0.0.1".into(),
            username: "test".into(),
            password: "test".into(),
            port,
            secure: false,
            auth_type: AuthType::Basic,
            insecure: false,
            app_servers: vec![],
            color: None,
            stripe_color_secondary: None,
            cluster: Default::default(),
            uri: None,
        })
        .unwrap()
    }

    fn snapshot_json() -> String {
        json!({
            "capturedAt":"2026-11-14T12:00:00Z", "groups":[], "hosts":[], "appServers":[],
            "databases":[{"id":"18446744073709551615", "name":"Orders", "forestIds":[],
                "securityDatabaseId":"0", "schemaDatabaseId":"0", "triggersDatabaseId":"0"}], "forests":[]
        }).to_string()
    }

    #[tokio::test]
    async fn reads_multipart_snapshot_without_rounding_ids() {
        let body = format!(
            "--boundary\r\nContent-Type: application/json\r\n\r\n{}\r\n--boundary--\r\n",
            snapshot_json()
        );
        let client = response_client("200 OK", &body).await;
        let snapshot = discover(&client).await.unwrap();
        assert_eq!(snapshot.databases[0].id, "18446744073709551615");
    }

    #[tokio::test]
    async fn reads_single_json_and_preserves_permission_errors() {
        let client = response_client("200 OK", &snapshot_json()).await;
        assert_eq!(discover(&client).await.unwrap().databases.len(), 1);
        let client = response_client(
            "403 Forbidden",
            r#"{"errorResponse":{"message":"SEC-PRIV: admin-module-read required"}}"#,
        )
        .await;
        let error = format!("{:#}", discover(&client).await.unwrap_err());
        assert!(error.contains("403"), "{error}");
        assert!(error.contains("admin-module-read"), "{error}");
    }
}

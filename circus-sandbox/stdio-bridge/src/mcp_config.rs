use std::collections::BTreeMap;
use std::path::PathBuf;

use circus_agent_runner::{McpConnection, McpToolPolicy};
use serde::Deserialize;

use crate::secrets::{KeychainRef, read_secret};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Connection {
    id: String,
    url: String,
    headers_file: Option<PathBuf>,
    headers_keychain: Option<KeychainRef>,
    ca_file: Option<PathBuf>,
    tools: Vec<McpToolPolicy>,
}

pub(crate) async fn load(configs: Vec<Connection>) -> Result<Vec<McpConnection>, ()> {
    if configs.len() > 8 {
        return Err(());
    }
    let mut connections = Vec::new();
    for config in configs {
        let headers = if config.headers_file.is_none() && config.headers_keychain.is_none() {
            BTreeMap::new()
        } else {
            let bytes = read_secret(
                config.headers_file.as_deref(),
                config.headers_keychain.as_ref(),
                16 * 1024,
            )
            .await?;
            sonic_rs::from_slice::<BTreeMap<String, String>>(&bytes).map_err(|_| ())?
        };
        connections.push(
            McpConnection::new(config.id, config.url, headers, config.ca_file, config.tools)
                .map_err(|_| ())?,
        );
    }
    Ok(connections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn credentials_are_host_references_and_errors_have_no_content() {
        let directory = tempfile::tempdir().unwrap();
        let headers = directory.path().join("headers.json");
        std::fs::write(&headers, r#"{"Authorization":"Bearer test-only-secret"}"#).unwrap();
        let json = format!(
            r#"{{"id":"test","url":"https://mcp.example/mcp","headersFile":{},"tools":[{{"name":"read","allowedArguments":[],"fixedArguments":{{"repo":"approved"}}}}]}}"#,
            sonic_rs::to_string(&headers).unwrap()
        );
        let connection: Connection = sonic_rs::from_str(&json).unwrap();
        let loaded = load(vec![connection]).await.unwrap();
        assert!(!format!("{loaded:?}").contains("test-only-secret"));
        assert!(
            sonic_rs::from_str::<Connection>(&json.replace("\"headersFile\"", "\"headers\""))
                .is_err()
        );
        std::fs::write(&headers, r#"{"Authorization":"line\r\ninjection"}"#).unwrap();
        let connection: Connection = sonic_rs::from_str(&json).unwrap();
        assert!(load(vec![connection]).await.is_err());
    }
}

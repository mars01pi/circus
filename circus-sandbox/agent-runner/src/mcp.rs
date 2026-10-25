//! Host-authorized MCP connections. None of these credentials enter the sandbox.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::TaskError;

/// One approved tool. Fixed arguments bind resource identifiers on the host.
/// The operator must review tool semantics; this is not a SQL/path policy engine.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpToolPolicy {
    pub name: String,
    pub allowed_arguments: Vec<String>,
    #[serde(default)]
    pub fixed_arguments: BTreeMap<String, String>,
}

/// A host-configured HTTPS Streamable HTTP endpoint and its tool policy.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConnection {
    pub(crate) id: String,
    pub(crate) url: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) ca_file: Option<PathBuf>,
    pub(crate) tools: Vec<McpToolPolicy>,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection").finish_non_exhaustive()
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl McpConnection {
    pub fn new(
        id: String,
        url: String,
        headers: BTreeMap<String, String>,
        ca_file: Option<PathBuf>,
        tools: Vec<McpToolPolicy>,
    ) -> Result<Self, TaskError> {
        let invalid = || TaskError::Invalid("MCP connection policy");
        let parsed = url::Url::parse(&url).map_err(|_| invalid())?;
        if !identifier(&id)
            || url.len() > 2048
            || parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || url.contains('\\')
            || url.bytes().any(|b| b <= 32)
            || url.split('/').any(|part| part == "." || part == "..")
            || ca_file.as_ref().is_some_and(|path| !path.is_absolute())
            || headers.len() > 16
            || tools.is_empty()
            || tools.len() > 32
        {
            return Err(invalid());
        }
        let mut header_names = BTreeSet::new();
        for (name, value) in &headers {
            let lower = name.to_ascii_lowercase();
            if !identifier(name)
                || !header_names.insert(lower.clone())
                || matches!(
                    lower.as_str(),
                    "host"
                        | "content-length"
                        | "content-type"
                        | "accept"
                        | "connection"
                        | "transfer-encoding"
                        | "mcp-session-id"
                        | "mcp-protocol-version"
                )
                || value.is_empty()
                || value.len() > 4096
                || !value.bytes().all(|b| (32..=126).contains(&b))
            {
                return Err(invalid());
            }
        }
        let mut names = BTreeSet::new();
        for tool in &tools {
            let arguments: BTreeSet<_> = tool.allowed_arguments.iter().collect();
            if !identifier(&tool.name)
                || !names.insert(&tool.name)
                || arguments.len() != tool.allowed_arguments.len()
                || arguments.len() + tool.fixed_arguments.len() > 32
                || arguments
                    .iter()
                    .any(|key| !identifier(key) || tool.fixed_arguments.contains_key(*key))
                || tool.fixed_arguments.iter().any(|(key, value)| {
                    !identifier(key) || value.len() > 1024 || value.chars().any(char::is_control)
                })
            {
                return Err(invalid());
            }
        }
        Ok(Self {
            id,
            url,
            headers,
            ca_file,
            tools,
        })
    }
}

/// A task can select only tools from an already configured connection.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpGrant {
    pub connection_id: String,
    pub tools: Vec<String>,
}

pub(crate) fn task_config(
    connections: &[McpConnection],
    grants: &[McpGrant],
) -> Result<String, TaskError> {
    if grants.len() > 8 {
        return Err(TaskError::Invalid("MCP grant count"));
    }
    let mut seen = BTreeSet::new();
    let mut selected = Vec::new();
    for grant in grants {
        let connection = connections
            .iter()
            .find(|c| c.id == grant.connection_id)
            .ok_or(TaskError::Invalid("unknown MCP connection"))?;
        let tools: BTreeSet<_> = grant.tools.iter().collect();
        if !seen.insert(&grant.connection_id)
            || tools.is_empty()
            || tools.len() != grant.tools.len()
            || tools
                .iter()
                .any(|name| !connection.tools.iter().any(|tool| &tool.name == *name))
        {
            return Err(TaskError::Invalid("MCP tool grant"));
        }
        let mut connection = connection.clone();
        connection.tools.retain(|tool| tools.contains(&tool.name));
        selected.push(connection);
    }
    let encoded =
        sonic_rs::to_string(&selected).map_err(|_| TaskError::Invalid("MCP configuration"))?;
    if encoded.len() > 48 * 1024 {
        return Err(TaskError::Invalid("MCP configuration byte budget"));
    }
    Ok(encoded)
}

pub(crate) fn validate_connections(connections: &[McpConnection]) -> Result<(), TaskError> {
    let mut seen = BTreeSet::new();
    if connections.len() > 8 || connections.iter().any(|c| !seen.insert(&c.id)) {
        return Err(TaskError::Invalid("MCP connection count or duplicate ID"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> McpConnection {
        McpConnection::new(
            "issues".into(),
            "https://mcp.example/mcp".into(),
            BTreeMap::from([("Authorization".into(), "Bearer host-secret".into())]),
            None,
            vec![McpToolPolicy {
                name: "read_issue".into(),
                allowed_arguments: vec!["number".into()],
                fixed_arguments: BTreeMap::from([("repo".into(), "approved".into())]),
            }],
        )
        .unwrap()
    }

    #[test]
    fn grants_never_imply_all_configured_tools() {
        let c = connection();
        assert_eq!(task_config(std::slice::from_ref(&c), &[]).unwrap(), "[]");
        for grant in [
            McpGrant {
                connection_id: "other".into(),
                tools: vec!["read_issue".into()],
            },
            McpGrant {
                connection_id: "issues".into(),
                tools: vec!["delete_issue".into()],
            },
        ] {
            assert!(task_config(std::slice::from_ref(&c), &[grant]).is_err());
        }
        assert!(!format!("{c:?}").contains("host-secret"));
    }

    #[test]
    fn host_scope_cannot_overlap_guest_arguments() {
        let c = connection();
        let mut tools = c.tools.clone();
        tools[0].allowed_arguments.push("repo".into());
        assert!(McpConnection::new(c.id, c.url, c.headers, None, tools).is_err());
        for name in ["Host", "Content-Length", "MCP-Session-Id", "Bad\r\nHeader"] {
            assert!(
                McpConnection::new(
                    "test".into(),
                    "https://mcp.example/mcp".into(),
                    BTreeMap::from([(name.into(), "value".into())]),
                    None,
                    connection().tools
                )
                .is_err()
            );
        }
    }
}

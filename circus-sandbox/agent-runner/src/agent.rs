/// A fixed, supported agent protocol and executable. This is not a shell command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentKind {
    #[default]
    Codex,
    ClaudeCode,
}

impl AgentKind {
    pub(crate) fn source(self) -> String {
        match self {
            Self::Codex => crate::runner::BRIDGE.to_owned(),
            Self::ClaudeCode => format!(
                "{}\n{}",
                crate::runner::BRIDGE,
                include_str!("claude_bridge.py")
            ),
        }
    }

    pub(crate) fn host(self) -> String {
        let entry = match self {
            Self::Codex => "main_host",
            Self::ClaudeCode => "main_claude_host",
        };
        format!(
            "{}\n{}\n{entry}()\n",
            self.source(),
            crate::runner::MCP_HOST
        )
    }

    pub(crate) fn guest(self) -> String {
        let entry = match self {
            Self::Codex => "main_guest",
            Self::ClaudeCode => "main_claude_guest",
        };
        format!("{}\n{entry}()\n", self.source())
    }
}

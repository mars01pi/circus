use serde::Deserialize;
use tokio::sync::mpsc;

use crate::{ModelFailure, TaskError, TaskEvent, ToolPhase};

pub(crate) const EVENT_QUEUE: usize = 256;
pub(crate) const MAX_EVENT_FRAME: usize = 64 * 1024;
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct Events {
    sender: mpsc::Sender<TaskEvent>,
    bytes: usize,
    session: Option<String>,
}

impl Events {
    pub(crate) fn new(sender: mpsc::Sender<TaskEvent>) -> Self {
        Self {
            sender,
            bytes: 0,
            session: None,
        }
    }

    /// Actual native agent session reported by the controller, if any.
    pub(crate) fn agent_session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    fn set_agent_session(&mut self, id: String) -> Result<(), TaskError> {
        if !crate::config::valid_session_id(&id) {
            return Err(TaskError::Protocol);
        }
        if let Some(existing) = &self.session {
            return if existing == &id {
                Ok(())
            } else {
                Err(TaskError::Protocol)
            };
        }
        self.session = Some(id);
        Ok(())
    }

    pub(crate) fn send(&mut self, event: TaskEvent) -> Result<(), TaskError> {
        let size = match &event {
            TaskEvent::TextDelta { text } => text.len(),
            TaskEvent::ToolOutput { id, text } => id.len() + text.len(),
            TaskEvent::Tool { id, kind, .. } => id.len() + kind.len(),
            TaskEvent::SandboxCreated { instance_id } => instance_id.len(),
            TaskEvent::Phase(_) => 0,
        } + 64;
        self.bytes = self
            .bytes
            .checked_add(size)
            .ok_or(TaskError::EventBackpressure)?;
        if self.bytes > MAX_EVENT_BYTES {
            return Err(TaskError::EventBackpressure);
        }
        self.sender
            .try_send(event)
            .map_err(|_| TaskError::EventBackpressure)
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WireEvent {
    Running,
    Text {
        text: String,
    },
    Tool {
        id: String,
        tool: String,
        phase: WireToolPhase,
        exit_code: Option<i64>,
    },
    ToolOutput {
        id: String,
        text: String,
    },
    TurnCompleted,
    /// Actual native thread id, reported once after start or resume.
    AgentSession {
        id: String,
    },
    Failure {
        code: FailureCode,
        status: Option<u16>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireToolPhase {
    Started,
    Completed,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureCode {
    Tls,
    Timeout,
    Rejected,
    Transport,
    Protocol,
    Budget,
    AgentFailed,
    UnsupportedInteraction,
    Controller,
}

impl WireEvent {
    pub(crate) fn apply(self, events: &mut Events) -> Result<bool, TaskError> {
        match self {
            Self::Running => events.send(TaskEvent::Phase(crate::TaskPhase::Running))?,
            Self::Text { text } => events.send(TaskEvent::TextDelta { text })?,
            Self::Tool {
                id,
                tool,
                phase,
                exit_code,
            } => events.send(TaskEvent::Tool {
                id,
                kind: tool,
                exit_code,
                phase: match phase {
                    WireToolPhase::Started => ToolPhase::Started,
                    WireToolPhase::Completed => ToolPhase::Completed,
                },
            })?,
            Self::ToolOutput { id, text } => events.send(TaskEvent::ToolOutput { id, text })?,
            Self::TurnCompleted => return Ok(true),
            Self::AgentSession { id } => events.set_agent_session(id)?,
            Self::Failure { code, status } => {
                return Err(match code {
                    FailureCode::Tls => ModelFailure::Tls.into(),
                    FailureCode::Timeout => ModelFailure::Timeout.into(),
                    FailureCode::Rejected => match status {
                        Some(status @ 100..=599) => ModelFailure::Rejected { status }.into(),
                        _ => TaskError::Protocol,
                    },
                    FailureCode::Transport => ModelFailure::Transport.into(),
                    FailureCode::Protocol => ModelFailure::Protocol.into(),
                    FailureCode::Budget => ModelFailure::Budget.into(),
                    FailureCode::AgentFailed => TaskError::AgentFailed,
                    FailureCode::UnsupportedInteraction => TaskError::UnsupportedInteraction,
                    FailureCode::Controller => TaskError::Protocol,
                });
            }
        }
        Ok(false)
    }
}

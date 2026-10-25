//! Aggregate stdout+stderr accounting for one engine exec.
//!
//! One request owns a single combined byte budget. Each stream additionally has
//! its own allowance: a command request gives both streams the same combined
//! budget, while the guest file helper gives stdout `max_bytes + 1` (to detect
//! an over-limit file) and stderr a fixed 4 KiB diagnostic allowance. The
//! combined limit is always the sum of those explicit allowances, never an
//! accidental per-stream doubling.

use super::{EngineError, EngineErrorKind};

/// Stream that would exceed its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputPhase {
    Stdout,
    Stderr,
}

/// Engine-level output budget violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputLimit {
    /// The violated budget in bytes.
    pub limit: usize,
    pub phase: OutputPhase,
}

impl OutputLimit {
    pub(crate) fn error(self) -> EngineError {
        EngineError {
            message: format!(
                "{} output would exceed the {}-byte budget",
                match self.phase {
                    OutputPhase::Stdout => "stdout",
                    OutputPhase::Stderr => "stderr",
                },
                self.limit
            ),
            not_found: false,
            kind: EngineErrorKind::Limit,
            create_pending: false,
            create_id: None,
            output: Some(self),
            policy: None,
        }
    }
}

/// Bounded collector for one exec.
///
/// `push` checks the stream allowance and the shared budget before copying any
/// byte, so a rejected chunk is never partially retained. Reaching a limit
/// exactly is accepted; the first byte over is rejected.
pub(crate) struct OutputCollector {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    combined: usize,
    stdout_limit: usize,
    stderr_limit: usize,
    combined_limit: usize,
}

impl OutputCollector {
    pub(crate) fn new(stdout_limit: usize, stderr_limit: usize, combined_limit: usize) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            combined: 0,
            stdout_limit,
            stderr_limit,
            combined_limit,
        }
    }

    pub(crate) fn push(&mut self, phase: OutputPhase, chunk: &[u8]) -> Result<(), EngineError> {
        let stream_limit = match phase {
            OutputPhase::Stdout => self.stdout_limit,
            OutputPhase::Stderr => self.stderr_limit,
        };
        let stream_len = match phase {
            OutputPhase::Stdout => self.stdout.len(),
            OutputPhase::Stderr => self.stderr.len(),
        };
        if stream_len.saturating_add(chunk.len()) > stream_limit {
            return Err(OutputLimit {
                limit: stream_limit,
                phase,
            }
            .error());
        }
        let combined_limit = self.combined_limit;
        let combined = self.combined.checked_add(chunk.len()).ok_or_else(|| {
            OutputLimit {
                limit: combined_limit,
                phase,
            }
            .error()
        })?;
        if combined > combined_limit {
            return Err(OutputLimit {
                limit: combined_limit,
                phase,
            }
            .error());
        }
        let buffer = match phase {
            OutputPhase::Stdout => &mut self.stdout,
            OutputPhase::Stderr => &mut self.stderr,
        };
        buffer.extend_from_slice(chunk);
        self.combined = combined;
        Ok(())
    }

    pub(crate) fn finish(self) -> (Vec<u8>, Vec<u8>) {
        (self.stdout, self.stderr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(error: EngineError) -> OutputLimit {
        error.output.expect("limit error carries its budget")
    }

    #[test]
    fn exact_combined_limit_is_allowed() {
        let mut collector = OutputCollector::new(10, 10, 10);
        collector.push(OutputPhase::Stdout, b"123456").unwrap();
        collector.push(OutputPhase::Stderr, b"7890").unwrap();
        let (stdout, stderr) = collector.finish();
        assert_eq!(stdout, b"123456");
        assert_eq!(stderr, b"7890");
    }

    #[test]
    fn first_byte_over_shared_budget_reports_stream_and_budget() {
        let mut collector = OutputCollector::new(10, 10, 10);
        collector.push(OutputPhase::Stdout, b"12345").unwrap();
        let error = collector
            .push(OutputPhase::Stderr, b"678901")
            .expect_err("combined budget exceeded");
        assert_eq!(
            limit(error),
            OutputLimit {
                limit: 10,
                phase: OutputPhase::Stderr,
            }
        );
    }

    #[test]
    fn file_helper_stderr_uses_its_own_allowance() {
        let mut collector = OutputCollector::new(5, 4, 9);
        collector.push(OutputPhase::Stdout, b"12345").unwrap();
        collector.push(OutputPhase::Stderr, b"abcd").unwrap();
        assert_eq!(
            limit(collector.push(OutputPhase::Stderr, b"e").unwrap_err()),
            OutputLimit {
                limit: 4,
                phase: OutputPhase::Stderr,
            }
        );
    }

    #[test]
    fn file_helper_stdout_detects_one_byte_over_the_file_bound() {
        let mut collector = OutputCollector::new(5, 4096, 4101);
        assert_eq!(
            limit(collector.push(OutputPhase::Stdout, b"123456").unwrap_err()),
            OutputLimit {
                limit: 5,
                phase: OutputPhase::Stdout,
            }
        );
    }

    #[test]
    fn oversized_chunk_is_not_partially_retained() {
        let mut collector = OutputCollector::new(4, 4, 8);
        let error = collector
            .push(OutputPhase::Stdout, b"12345")
            .expect_err("stream bound exceeded");
        assert!(matches!(
            error.output,
            Some(OutputLimit {
                limit: 4,
                phase: OutputPhase::Stdout
            })
        ));
        let (stdout, stderr) = collector.finish();
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }
}

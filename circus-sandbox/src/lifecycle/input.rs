//! Matchable request validation failures, independent of backend SDK types.

use std::time::Duration;

/// Invalid command input rejected before an exec is queued or accepted.
///
/// These failures leave the sandbox and its owned container unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// An argv vector must contain at least the executable.
    #[error("argv must contain an executable")]
    EmptyArgv,
    /// The supplied command timeout must be non-zero.
    #[error("command timeout must be non-zero, got {timeout:?}")]
    InvalidTimeout { timeout: Duration },
    /// The supplied output byte bound must be non-zero.
    #[error("command output limit must be non-zero, got {output_limit_bytes}")]
    InvalidOutputLimit { output_limit_bytes: usize },
}

/// Staged file payload sizes could not fit the configured workspace byte bound.
///
/// This is a pre-transfer check of metadata lengths, not a host disk quota or an
/// estimate of tmpfs metadata and allocation overhead. Staging is retained and
/// no guest payload has been sent when it fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StagingError {
    /// The first observed sum above `limit`; `actual` is a lower bound on all
    /// staged file payload bytes, not necessarily the complete staged total.
    #[error("staged file payloads exceed {limit} bytes: observed at least {actual}")]
    LimitExceeded { limit: u64, actual: u64 },
    /// Adding the next payload length could not be represented as `u64`.
    ///
    /// `accumulated` counts preceding payloads, `next` is the current payload
    /// length, and `limit` is the configured workspace byte bound.
    #[error("staged byte count overflow: {accumulated} + {next}, workspace bound {limit}")]
    SizeOverflow {
        accumulated: u64,
        next: u64,
        limit: u64,
    },
}

pub(super) fn staged_total(accumulated: u64, next: u64, limit: u64) -> Result<u64, StagingError> {
    let actual = accumulated
        .checked_add(next)
        .ok_or(StagingError::SizeOverflow {
            accumulated,
            next,
            limit,
        })?;
    if actual > limit {
        return Err(StagingError::LimitExceeded { limit, actual });
    }
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_count_preserves_limit_and_detected_total() {
        assert_eq!(
            staged_total(700, 700, 1024),
            Err(StagingError::LimitExceeded {
                limit: 1024,
                actual: 1400,
            })
        );
        assert_eq!(staged_total(700, 324, 1024), Ok(1024));
    }

    #[test]
    fn staging_count_preserves_overflow_operands_and_bound() {
        assert_eq!(
            staged_total(u64::MAX, 1, u64::MAX),
            Err(StagingError::SizeOverflow {
                accumulated: u64::MAX,
                next: 1,
                limit: u64::MAX,
            })
        );
    }
}

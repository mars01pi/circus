use std::collections::BTreeMap;
use std::time::Duration;

use super::*;

fn request() -> ProcessRequest {
    ProcessRequest {
        argv: vec!["agent".into(), "".into()],
        working_dir: Some("project/src".into()),
        env: BTreeMap::from([("HOME".into(), "/workspace/.agent".into())]),
        timeout: Duration::from_secs(30),
        output_limit_bytes: 1024,
    }
}

#[test]
fn validates_guest_launch_without_exposing_environment_values() {
    let mut valid = request();
    valid
        .env
        .insert("TOKEN".into(), "private-test-value".into());
    valid.argv.push("private-prompt".into());
    valid.validate().unwrap();
    let debug = format!("{valid:?}");
    assert!(!debug.contains("private-test-value"));
    assert!(!debug.contains("private-prompt"));
    assert!(debug.contains("TOKEN"));
}

#[test]
fn rejects_malformed_requests_before_transport() {
    for argv in [vec![], vec!["".into()], vec!["agent\0argument".into()]] {
        assert!(ProcessRequest { argv, ..request() }.validate().is_err());
    }
    for timeout in [Duration::ZERO, Duration::MAX] {
        assert!(
            ProcessRequest {
                timeout,
                ..request()
            }
            .validate()
            .is_err()
        );
    }
    for output_limit_bytes in [0, usize::MAX] {
        assert!(
            ProcessRequest {
                output_limit_bytes,
                ..request()
            }
            .validate()
            .is_err()
        );
    }
    for path in ["", "../out", "/tmp", "a/C:secret", "a\\b", "a\0b"] {
        assert!(
            ProcessRequest {
                working_dir: Some(path.into()),
                ..request()
            }
            .validate()
            .is_err()
        );
    }
    for (key, value) in [("", "v"), ("A=B", "v"), ("1BAD", "v"), ("OK", "a\0b")] {
        assert!(
            ProcessRequest {
                env: BTreeMap::from([(key.into(), value.into())]),
                ..request()
            }
            .validate()
            .is_err()
        );
    }
}

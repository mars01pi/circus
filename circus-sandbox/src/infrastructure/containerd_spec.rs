//! OCI runtime-spec construction and inspected-spec reporting for containerd.
//!
//! The spec is plain JSON (the OCI runtime-spec is JSON by definition) placed
//! in a protobuf `Any`. Nothing here is a containerd SDK type; only the `Any`
//! envelope is needed to submit it.

use std::collections::BTreeMap;

use prost_types::Any;
use serde_json::{Value, json};

use super::backend::CommonSpec;
use super::engine::{EngineError, EngineErrorKind, HostReport, file_error};

pub(crate) const SPEC_TYPE_URL: &str = "types.containerd.io/opencontainers/runtime-spec/1/Spec";
pub(crate) const PROCESS_TYPE_URL: &str =
    "types.containerd.io/opencontainers/runtime-spec/1/Process";

const DEFAULT_PATH: &str = "/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin";
const WORKSPACE_MODE: u32 = 0o755;
const CPU_PERIOD: u64 = 100_000;

/// Builds the one container init spec. The init process parks in `sleep` so the
/// sandbox stays alive until lifecycle starts, pauses, resumes, or destroys it.
pub(crate) fn container_any(
    spec: &CommonSpec,
    hostname: &str,
    image_env: &[String],
) -> Result<Any, EngineError> {
    let env = merged_env(image_env.iter().map(String::as_str), &[])?;
    let args = vec![
        "python3".to_string(),
        "-I".to_string(),
        "-B".to_string(),
        "-c".to_string(),
        "import time; time.sleep(31536000)".to_string(),
    ];
    let process = process_value(
        spec.guest_uid,
        spec.guest_gid,
        &spec.workspace_path,
        &args,
        &env,
    );
    let spec = json!({
        "ociVersion": "1.0.2",
        "process": process,
        "root": {"path": "rootfs", "readonly": true},
        "hostname": hostname,
        "mounts": system_mounts(spec),
        "linux": {
            "namespaces": [
                {"type": "pid"},
                {"type": "ipc"},
                {"type": "uts"},
                {"type": "mount"},
                {"type": "network"},
                {"type": "cgroup"},
            ],
            "resources": {
                "memory": {
                    "limit": spec.memory,
                    // Equal to the limit: no swap beyond the memory bound.
                    "swap": spec.memory,
                },
                "cpu": {
                    "quota": spec.nano_cpus / 10_000,
                    "period": CPU_PERIOD,
                },
                "pids": {"limit": spec.pids},
                "devices": device_rules(),
            },
            "devices": devices(spec.guest_uid, spec.guest_gid),
            "maskedPaths": [
                "/proc/acpi",
                "/proc/asound",
                "/proc/kcore",
                "/proc/keys",
                "/proc/latency_stats",
                "/proc/timer_list",
                "/proc/timer_stats",
                "/proc/sched_debug",
                "/sys/firmware",
                "/proc/scsi",
            ],
            "readonlyPaths": [
                "/proc/bus",
                "/proc/fs",
                "/proc/irq",
                "/proc/sys",
                "/proc/sysrq-trigger",
            ],
        },
    });
    Ok(any(SPEC_TYPE_URL, &spec))
}

/// Builds one exec process spec. `user` is the lifecycle-owned `uid:gid` pair.
pub(crate) fn process_any(
    user: &str,
    workdir: &str,
    argv: &[String],
    env: &[String],
    container_spec: Option<&Any>,
) -> Result<Any, EngineError> {
    let (uid, gid) = parse_user(user)?;
    let stored = container_spec
        .filter(|spec| spec.type_url == SPEC_TYPE_URL)
        .ok_or_else(|| file_error(EngineErrorKind::Other, "container has no OCI spec"))?;
    let stored: Value = serde_json::from_slice(&stored.value).map_err(|err| {
        file_error(
            EngineErrorKind::Other,
            format!("container spec is not JSON: {err}"),
        )
    })?;
    let base = stored
        .pointer("/process/env")
        .ok_or_else(|| file_error(EngineErrorKind::Other, "container spec has no process env"))?;
    let base = env_values(base)?;
    let env = merged_env(base.iter().filter_map(Value::as_str), env)?;
    let process = process_value(uid, gid, workdir, argv, &env);
    Ok(any(PROCESS_TYPE_URL, &process))
}

fn any(type_url: &str, value: &Value) -> Any {
    Any {
        type_url: type_url.to_string(),
        value: value.to_string().into_bytes(),
    }
}

fn process_value(uid: u32, gid: u32, cwd: &str, argv: &[String], env: &[String]) -> Value {
    json!({
        "terminal": false,
        "user": {"uid": uid, "gid": gid, "additionalGids": []},
        "args": argv,
        "env": env,
        "cwd": cwd,
        "capabilities": {
            "bounding": [],
            "effective": [],
            "inheritable": [],
            "permitted": [],
            "ambient": [],
        },
        "noNewPrivileges": true,
    })
}

/// Validates metadata before callers allocate an owned environment.
pub(super) fn env_values(value: &Value) -> Result<&[Value], EngineError> {
    let values = value
        .as_array()
        .ok_or_else(|| file_error(EngineErrorKind::Other, "environment is not an array"))?;
    for value in values {
        let entry = value.as_str().ok_or_else(|| {
            file_error(EngineErrorKind::Other, "environment entry is not a string")
        })?;
        env_key(entry)?;
    }
    Ok(values)
}

fn env_key(entry: &str) -> Result<&str, EngineError> {
    entry
        .split_once('=')
        .map(|(key, _)| key)
        .filter(|key| !key.is_empty() && !entry.contains('\0'))
        .ok_or_else(|| file_error(EngineErrorKind::Other, "invalid environment entry"))
}

/// Last value per key wins; caller overrides beat image values. Defaults only
/// fill missing keys, including when a supplied value is explicitly empty.
fn merged_env<'a>(
    base: impl DoubleEndedIterator<Item = &'a str> + Clone,
    explicit: &'a [String],
) -> Result<Vec<String>, EngineError> {
    for entry in base.clone().chain(explicit.iter().map(String::as_str)) {
        env_key(entry)?;
    }
    let mut env: Vec<String> = Vec::with_capacity(base.clone().count() + explicit.len() + 2);
    for entry in explicit.iter().map(String::as_str).rev().chain(base.rev()) {
        let key = env_key(entry)?;
        if !env
            .iter()
            .any(|existing| existing.split_once('=').map(|(key, _)| key) == Some(key))
        {
            env.push(entry.to_string());
        }
    }
    env.reverse();
    if !env.iter().any(|entry| entry.starts_with("PATH=")) {
        env.push(format!("PATH={DEFAULT_PATH}"));
    }
    if !env.iter().any(|entry| entry.starts_with("LANG=")) {
        env.push("LANG=C.UTF-8".to_string());
    }
    Ok(env)
}

/// Parses the lifecycle-owned `uid:gid` user string into numeric ids.
pub(crate) fn parse_user(user: &str) -> Result<(u32, u32), EngineError> {
    let invalid = || {
        file_error(
            EngineErrorKind::Other,
            format!("guest user {user:?} is not a numeric uid:gid pair"),
        )
    };
    let (uid, gid) = user.split_once(':').ok_or_else(invalid)?;
    Ok((
        uid.parse().map_err(|_| invalid())?,
        gid.parse().map_err(|_| invalid())?,
    ))
}

fn system_mounts(spec: &CommonSpec) -> Vec<Value> {
    vec![
        json!({
            "destination": "/proc",
            "type": "proc",
            "source": "proc",
            "options": ["nosuid", "noexec", "nodev"],
        }),
        json!({
            "destination": "/dev",
            "type": "tmpfs",
            "source": "tmpfs",
            "options": ["nosuid", "strictatime", "mode=755", "size=65536k"],
        }),
        json!({
            "destination": "/dev/pts",
            "type": "devpts",
            "source": "devpts",
            "options": [
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
                "gid=5",
            ],
        }),
        json!({
            "destination": "/dev/shm",
            "type": "tmpfs",
            "source": "shm",
            "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"],
        }),
        json!({
            "destination": "/sys",
            "type": "sysfs",
            "source": "sysfs",
            "options": ["nosuid", "noexec", "nodev", "ro"],
        }),
        json!({
            "destination": "/sys/fs/cgroup",
            "type": "cgroup",
            "source": "cgroup",
            "options": ["nosuid", "noexec", "nodev", "relatime", "ro"],
        }),
        json!({
            "destination": spec.workspace_path,
            "type": "tmpfs",
            "source": "tmpfs",
            "options": [
                "rw",
                "nosuid",
                "nodev",
                format!("uid={}", spec.guest_uid),
                format!("gid={}", spec.guest_gid),
                format!("mode={WORKSPACE_MODE:o}"),
                format!("size={}", spec.workspace_bytes),
            ],
        }),
    ]
}

fn devices(uid: u32, gid: u32) -> Vec<Value> {
    [
        ("/dev/null", 1, 3, 0o666),
        ("/dev/zero", 1, 5, 0o666),
        ("/dev/full", 1, 7, 0o666),
        ("/dev/random", 1, 8, 0o666),
        ("/dev/urandom", 1, 9, 0o666),
        ("/dev/tty", 5, 0, 0o666),
        ("/dev/ptmx", 5, 2, 0o666),
    ]
    .into_iter()
    .map(|(path, major, minor, mode)| {
        json!({
            "path": path,
            "type": "c",
            "major": major,
            "minor": minor,
            "fileMode": mode,
            "uid": uid,
            "gid": gid,
        })
    })
    .collect()
}

/// Device cgroup policy: deny by default, then allow the standard pseudo
/// devices every Linux container needs. Without the explicit allow rules the
/// cgroup v2 eBPF device controller rejects reopening `/dev/null` at start.
fn device_rules() -> Vec<Value> {
    let mut rules = vec![json!({"allow": false, "access": "rwm"})];
    let allowed = [
        ("c", Some(1), Some(3)),
        ("c", Some(1), Some(5)),
        ("c", Some(1), Some(7)),
        ("c", Some(1), Some(8)),
        ("c", Some(1), Some(9)),
        ("c", Some(5), Some(0)),
        ("c", Some(5), Some(1)),
        ("c", Some(5), Some(2)),
        ("c", Some(136), None),
        ("c", Some(10), Some(200)),
        ("c", Some(10), Some(229)),
        ("b", Some(7), None),
    ];
    for (kind, major, minor) in allowed {
        let mut rule = serde_json::Map::new();
        rule.insert("allow".to_string(), json!(true));
        rule.insert("type".to_string(), json!(kind));
        if let Some(major) = major {
            rule.insert("major".to_string(), json!(major));
        }
        if let Some(minor) = minor {
            rule.insert("minor".to_string(), json!(minor));
        }
        rule.insert("access".to_string(), json!("rwm"));
        rules.push(Value::Object(rule));
    }
    rules
}

/// Reads back the spec this adapter asked containerd to store.
///
/// Only structural facts are reported. containerd exposes no effective cgroup
/// controller values, so memory, CPU and PID numbers are left absent. Values
/// here are the stored OCI spec (what runc receives), not a kernel observation.
pub(crate) fn report_from_any(runtime: Option<String>, spec: Option<&Any>) -> HostReport {
    let mut report = HostReport {
        runtime,
        ..HostReport::default()
    };
    let Some(any) = spec else {
        return report;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&any.value) else {
        return report;
    };
    report.readonly_root = value.pointer("/root/readonly").and_then(Value::as_bool);
    report.cap_drop =
        empty_capabilities(value.pointer("/process/capabilities")).then(|| vec!["ALL".to_string()]);
    report.security_opt = match value
        .pointer("/process/noNewPrivileges")
        .and_then(Value::as_bool)
    {
        Some(true) => Some(vec!["no-new-privileges".to_string()]),
        _ => None,
    };
    report.network_mode = network_mode(&value).map(str::to_string);
    if let Some(mounts) = value.get("mounts").and_then(Value::as_array) {
        report.bind_count = Some(
            mounts
                .iter()
                .filter(|mount| mount.get("type").and_then(Value::as_str) == Some("bind"))
                .count(),
        );
        let mut tmpfs = BTreeMap::new();
        for mount in mounts {
            if mount.get("type").and_then(Value::as_str) != Some("tmpfs") {
                continue;
            }
            let Some(destination) = mount.get("destination").and_then(Value::as_str) else {
                continue;
            };
            // `/dev` and `/dev/shm` are runtime plumbing, not the workspace.
            if destination == "/dev" || destination == "/dev/shm" {
                continue;
            }
            let options = mount
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            tmpfs.insert(destination.to_string(), options);
        }
        report.tmpfs = Some(tmpfs);
    }
    report
}

fn empty_capabilities(capabilities: Option<&Value>) -> bool {
    let Some(object) = capabilities.and_then(Value::as_object) else {
        return false;
    };
    [
        "bounding",
        "effective",
        "inheritable",
        "permitted",
        "ambient",
    ]
    .iter()
    .all(|key| {
        object
            .get(*key)
            .and_then(Value::as_array)
            .is_none_or(|items| items.is_empty())
    })
}

/// This adapter installs no CNI or OCI hooks. A pathless network namespace is
/// fresh and unconfigured, not an inherited or externally joined namespace.
/// Host-privileged changes after creation are outside this inspected-spec report.
fn network_mode(value: &Value) -> Option<&'static str> {
    let namespaces = value.pointer("/linux/namespaces")?.as_array()?;
    let mut network = None;
    for namespace in namespaces {
        let kind = namespace.get("type")?.as_str()?;
        if kind.is_empty() {
            return None;
        }
        if kind == "network" {
            if network.is_some() {
                return None;
            }
            network = Some(namespace);
        }
    }
    let Some(network) = network else {
        return Some("host");
    };
    match network.get("path") {
        Some(Value::String(path)) if !path.is_empty() => Some("private"),
        Some(_) => None,
        None => {
            // Any hook can configure networking; executable names are not proof.
            let no_hooks = match value.get("hooks") {
                None => true,
                Some(Value::Object(hooks)) => hooks
                    .values()
                    .all(|hooks| hooks.as_array().is_some_and(Vec::is_empty)),
                Some(_) => false,
            };
            no_hooks.then_some("none")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CommonSpec {
        CommonSpec {
            memory: 64 * 1024 * 1024,
            nano_cpus: 100_000_000,
            pids: 32,
            workspace_path: "/workspace".to_string(),
            workspace_bytes: 8 * 1024 * 1024,
            guest_uid: 65_534,
            guest_gid: 65_534,
        }
    }

    #[test]
    fn spec_is_read_only_capability_free_and_workspace_bounded() {
        let any = container_any(&spec(), "circus", &[]).unwrap();
        assert_eq!(any.type_url, SPEC_TYPE_URL);
        let value: Value = serde_json::from_slice(&any.value).unwrap();
        assert_eq!(value.pointer("/root/readonly"), Some(&Value::Bool(true)));
        assert_eq!(
            value.pointer("/process/noNewPrivileges"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            value.pointer("/linux/resources/pids/limit"),
            Some(&json!(32))
        );
        assert_eq!(
            value.pointer("/linux/resources/cpu/quota"),
            Some(&json!(10_000))
        );
        let mounts = value.get("mounts").and_then(Value::as_array).unwrap();
        let workspace = mounts
            .iter()
            .find(|mount| mount.get("destination").and_then(Value::as_str) == Some("/workspace"))
            .unwrap();
        let options = workspace.get("options").unwrap().as_array().unwrap();
        for required in [
            "rw",
            "nosuid",
            "nodev",
            "uid=65534",
            "gid=65534",
            "mode=755",
        ] {
            assert!(
                options
                    .iter()
                    .any(|option| option.as_str() == Some(required)),
                "missing {required}"
            );
        }
        assert!(
            options
                .iter()
                .any(|option| option.as_str() == Some("size=8388608"))
        );
    }

    #[test]
    fn report_reads_back_structural_facts_only() {
        let any = container_any(&spec(), "circus", &[]).unwrap();
        let report = report_from_any(Some("io.containerd.runc.v2".to_string()), Some(&any));
        assert_eq!(report.runtime.as_deref(), Some("io.containerd.runc.v2"));
        assert_eq!(report.readonly_root, Some(true));
        assert_eq!(report.network_mode.as_deref(), Some("none"));
        assert_eq!(report.cap_drop, Some(vec!["ALL".to_string()]));
        assert_eq!(
            report.security_opt,
            Some(vec!["no-new-privileges".to_string()])
        );
        assert_eq!(report.bind_count, Some(0));
        assert_eq!(report.memory_bytes, None);
        assert_eq!(report.nano_cpus, None);
        assert_eq!(report.pids_limit, None);
        let tmpfs = report.tmpfs.unwrap();
        assert_eq!(tmpfs.len(), 1);
        assert!(tmpfs.contains_key("/workspace"));
    }

    #[test]
    fn network_report_never_calls_joined_or_unknown_namespaces_offline() {
        let report =
            |value: Value| report_from_any(None, Some(&any(SPEC_TYPE_URL, &value))).network_mode;
        assert_eq!(
            report(json!({"linux":{"namespaces":[{"type":"network"}]}})).as_deref(),
            Some("none")
        );
        assert_eq!(
            report(json!({"linux":{"namespaces":[{"type":"network","path":"/run/netns/joined"}]}}))
                .as_deref(),
            Some("private")
        );
        assert_eq!(
            report(json!({"linux":{"namespaces":[{"type":"pid"}]}})).as_deref(),
            Some("host")
        );
        for namespaces in [
            Value::Null,
            json!({}),
            json!([null]),
            json!([{}]),
            json!([{"type":null}]),
            json!([{"type":""},{"type":"network"}]),
            json!([{"type":"network"},{"type":"network"}]),
            json!([{"type":"network","path":null}]),
            json!([{"type":"network","path":""}]),
            json!([{"type":"network","path":12}]),
        ] {
            assert_eq!(report(json!({"linux":{"namespaces":namespaces}})), None);
        }
        for hooks in [
            Value::Null,
            json!([]),
            json!({"createRuntime":null}),
            json!({"createRuntime":[{"path":"/arbitrary-hook"}]}),
        ] {
            assert_eq!(
                report(json!({"linux":{"namespaces":[{"type":"network"}]},"hooks":hooks})),
                None
            );
        }
    }

    #[test]
    fn process_spec_carries_explicit_env_and_numeric_user() {
        let any = process_any(
            "65534:65534",
            "/workspace",
            &["python3".to_string()],
            &["A=1".to_string()],
            Some(&container_any(&spec(), "circus", &[]).unwrap()),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&any.value).unwrap();
        let env = value.get("env").unwrap().as_array().unwrap();
        assert!(env.contains(&json!("A=1")));
        assert!(
            env.iter()
                .any(|entry| entry.as_str().is_some_and(|v| v.starts_with("PATH=")))
        );
        assert_eq!(value.pointer("/user/uid"), Some(&json!(65_534)));
    }

    #[test]
    fn init_inherits_image_environment_without_changing_lifecycle_fields() {
        let image_env = [
            "HOME=/workspace",
            "CODEX_HOME=/workspace/codex",
            "PATH=/image/bin",
            "LANG=",
        ]
        .map(str::to_string);
        let spec = container_any(&spec(), "circus", &image_env).unwrap();
        let value: Value = serde_json::from_slice(&spec.value).unwrap();
        assert_eq!(value.pointer("/process/env"), Some(&json!(image_env)));
        assert_eq!(value.pointer("/process/cwd"), Some(&json!("/workspace")));
        assert_eq!(value.pointer("/process/user/uid"), Some(&json!(65_534)));
        assert_eq!(value.pointer("/process/args/0"), Some(&json!("python3")));
    }

    #[test]
    fn exec_overrides_inherited_keys_once_and_keeps_unspecified_image_values() {
        let image_env = [
            "HOME=/old",
            "HOME=/workspace",
            "CODEX_HOME=/workspace/codex",
            "PATH=/image/bin",
            "LANG=image-locale",
            "WITH_EQUALS=a=b",
        ]
        .map(str::to_string);
        let container = container_any(&spec(), "circus", &image_env).unwrap();
        let process = process_any(
            "65534:65534",
            "/workspace/subdir",
            &["custom-command".to_string()],
            &[
                "PATH=/ignored".to_string(),
                "PATH=/caller/bin".to_string(),
                "LANG=".to_string(),
            ],
            Some(&container),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&process.value).unwrap();
        assert_eq!(
            value.get("env"),
            Some(&json!([
                "HOME=/workspace",
                "CODEX_HOME=/workspace/codex",
                "WITH_EQUALS=a=b",
                "PATH=/caller/bin",
                "LANG=",
            ]))
        );
        assert_eq!(value.get("cwd"), Some(&json!("/workspace/subdir")));
        assert_eq!(value.get("args"), Some(&json!(["custom-command"])));
    }

    #[test]
    fn environment_defaults_only_fill_absent_keys() {
        let env = merged_env(["HOME=/workspace", "PATH="].into_iter(), &[]).unwrap();
        assert_eq!(env, ["HOME=/workspace", "PATH=", "LANG=C.UTF-8"]);
        let env = merged_env(std::iter::empty(), &[]).unwrap();
        assert_eq!(
            env,
            [format!("PATH={DEFAULT_PATH}"), "LANG=C.UTF-8".to_string()]
        );
    }

    #[test]
    fn malformed_container_environment_fails_closed_even_with_overrides() {
        let execute = |stored: Option<&Any>| {
            process_any(
                "65534:65534",
                "/workspace",
                &["true".to_string()],
                &["HOME=/override".to_string()],
                stored,
            )
        };
        assert!(execute(None).is_err());
        assert!(execute(Some(&any(PROCESS_TYPE_URL, &json!({"process":{"env":[]}})))).is_err());
        assert!(
            execute(Some(&Any {
                type_url: SPEC_TYPE_URL.to_string(),
                value: b"not json".to_vec(),
            }))
            .is_err()
        );
        for stored in [
            json!({}),
            json!({"process":null}),
            json!({"process":{"env":null}}),
            json!({"process":{"env":{}}}),
            json!({"process":{"env":[1]}}),
            json!({"process":{"env":["HOME"]}}),
            json!({"process":{"env":["=empty-key"]}}),
            json!({"process":{"env":["HOME=/bad\0value"]}}),
        ] {
            assert!(execute(Some(&any(SPEC_TYPE_URL, &stored))).is_err());
        }
    }

    #[test]
    fn invalid_image_and_explicit_environment_entries_are_rejected() {
        for entry in [
            "missing-equals",
            "=empty-key",
            "KEY=bad\0value",
            "BAD\0KEY=value",
        ] {
            assert!(container_any(&spec(), "circus", &[entry.to_string()]).is_err());
            let container = container_any(&spec(), "circus", &[]).unwrap();
            assert!(
                process_any(
                    "65534:65534",
                    "/workspace",
                    &["true".to_string()],
                    &[entry.to_string()],
                    Some(&container),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn non_numeric_user_is_rejected() {
        assert!(
            process_any(
                "nobody",
                "/workspace",
                &["true".to_string()],
                &[],
                Some(&container_any(&spec(), "circus", &[]).unwrap()),
            )
            .is_err()
        );
    }
}

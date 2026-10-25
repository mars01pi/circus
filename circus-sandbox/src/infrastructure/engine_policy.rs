//! Inspected effective-policy reporting and post-create verification.
//!
//! Requested values are never treated as evidence. Verification compares the
//! engine's inspected `HostConfig` against the values the adapter constructed;
//! any difference is a typed [`PolicyMismatch`] so creation fails rather than
//! reporting a policy that the engine did not preserve.

use std::collections::{BTreeMap, HashMap};

use super::CreateSpec;

/// Container label that proves ownership before any destructive operation.
pub(crate) const OWNER_LABEL_KEY: &str = "circus.sandbox.owner";

/// Size of `/dev/shm` requested for every sandbox.
pub(crate) const SHM_SIZE: i64 = 65_536;
const NETWORK_MODE: &str = "none";
const IPC_MODE: &str = "private";
const CGROUPNS_MODE: &str = "private";
const SECURITY_OPT: &str = "no-new-privileges:true";
const RESTART_POLICY: &str = "no:0";

/// Inspected effective configuration. A `None` field means the backend did not
/// report it; it is never replaced with a default that could look enforced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct HostReport {
    pub runtime: Option<String>,
    pub memory_bytes: Option<i64>,
    pub memory_swap_bytes: Option<i64>,
    pub nano_cpus: Option<i64>,
    pub pids_limit: Option<i64>,
    pub readonly_root: Option<bool>,
    pub network_mode: Option<String>,
    pub cap_drop: Option<Vec<String>>,
    pub cap_add: Option<Vec<String>>,
    pub security_opt: Option<Vec<String>>,
    /// Absent means the engine reported no host binds.
    pub bind_count: Option<usize>,
    /// Absent means the engine reported no host or volume mounts.
    pub mount_count: Option<usize>,
    /// Absent means the engine reported no device mappings.
    pub device_count: Option<usize>,
    /// Effective tmpfs options keyed by guest path.
    pub tmpfs: Option<BTreeMap<String, String>>,
    pub init: Option<bool>,
    pub ipc_mode: Option<String>,
    pub cgroupns_mode: Option<String>,
    pub oom_kill_disable: Option<bool>,
    pub auto_remove: Option<bool>,
    /// `name:maximum_retry_count`.
    pub restart_policy: Option<String>,
    pub shm_size: Option<i64>,
}

/// Builds the create request. Every field here has a matching verification in
/// [`verify`]; a value added here without verification must not claim
/// enforcement.
pub(crate) fn create_body(name: &str, spec: &CreateSpec) -> bollard::models::ContainerCreateBody {
    let mut labels = HashMap::new();
    labels.insert(OWNER_LABEL_KEY.to_string(), spec.owner_label.clone());
    labels.insert("circus.sandbox.name".to_string(), name.to_string());
    let mut tmpfs = HashMap::new();
    tmpfs.insert(
        spec.workspace_path.clone(),
        format!(
            "rw,nosuid,nodev,uid={},gid={},mode=0755,size={}",
            spec.guest_uid, spec.guest_gid, spec.workspace_bytes
        ),
    );
    bollard::models::ContainerCreateBody {
        image: Some(spec.image.clone()),
        user: Some(format!("{}:{}", spec.guest_uid, spec.guest_gid)),
        tty: Some(false),
        open_stdin: Some(false),
        attach_stdin: Some(false),
        env: Some(Vec::new()),
        cmd: Some(Vec::new()),
        entrypoint: Some(vec![
            "python3".to_string(),
            "-I".to_string(),
            "-B".to_string(),
            "-c".to_string(),
            "import time; time.sleep(31536000)".to_string(),
        ]),
        working_dir: Some(spec.workspace_path.clone()),
        network_disabled: Some(true),
        exposed_ports: Some(Vec::new()),
        volumes: Some(Vec::new()),
        labels: Some(labels),
        stop_signal: Some("SIGKILL".to_string()),
        stop_timeout: Some(1),
        host_config: Some(bollard::models::HostConfig {
            memory: Some(spec.memory),
            memory_swap: Some(spec.memory),
            nano_cpus: Some(spec.nano_cpus),
            pids_limit: Some(spec.pids),
            readonly_rootfs: Some(true),
            network_mode: Some(NETWORK_MODE.to_string()),
            cap_drop: Some(vec!["ALL".to_string()]),
            cap_add: Some(Vec::new()),
            security_opt: Some(vec![SECURITY_OPT.to_string()]),
            privileged: Some(false),
            publish_all_ports: Some(false),
            binds: Some(Vec::new()),
            mounts: Some(Vec::new()),
            tmpfs: Some(tmpfs),
            runtime: Some(spec.runtime.clone()),
            auto_remove: Some(false),
            init: Some(true),
            ipc_mode: Some(IPC_MODE.to_string()),
            cgroupns_mode: Some(bollard::models::HostConfigCgroupnsModeEnum::PRIVATE),
            oom_kill_disable: Some(false),
            shm_size: Some(SHM_SIZE),
            restart_policy: Some(bollard::models::RestartPolicy {
                name: Some(bollard::models::RestartPolicyNameEnum::NO),
                maximum_retry_count: Some(0),
            }),
            devices: Some(Vec::new()),
            extra_hosts: Some(Vec::new()),
            dns: Some(Vec::new()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// One inspected field that differs from the constructed policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PolicyMismatch {
    pub field: &'static str,
    pub expected: String,
    pub actual: String,
}

impl PolicyMismatch {
    pub(crate) fn error(self) -> super::EngineError {
        super::EngineError {
            message: format!(
                "engine effective policy mismatch for {}: expected {}, got {}",
                self.field, self.expected, self.actual
            ),
            not_found: false,
            kind: super::EngineErrorKind::Other,
            create_pending: false,
            create_id: None,
            output: None,
            policy: Some(Box::new(self)),
        }
    }
}

/// Builds a report from the inspected host configuration.
pub(crate) fn report_from(host: Option<bollard::models::HostConfig>) -> HostReport {
    let Some(host) = host else {
        return HostReport::default();
    };
    HostReport {
        runtime: host.runtime,
        memory_bytes: host.memory,
        memory_swap_bytes: host.memory_swap,
        nano_cpus: host.nano_cpus,
        pids_limit: host.pids_limit,
        readonly_root: host.readonly_rootfs,
        network_mode: host.network_mode,
        cap_drop: host.cap_drop,
        cap_add: host.cap_add,
        security_opt: host.security_opt,
        bind_count: host.binds.map(|binds| binds.len()),
        mount_count: host.mounts.map(|mounts| mounts.len()),
        device_count: host.devices.map(|devices| devices.len()),
        tmpfs: host.tmpfs.map(|tmpfs| tmpfs.into_iter().collect()),
        init: host.init,
        ipc_mode: host.ipc_mode,
        cgroupns_mode: host.cgroupns_mode.map(|mode| mode.to_string()),
        oom_kill_disable: host.oom_kill_disable,
        auto_remove: host.auto_remove,
        restart_policy: host.restart_policy.map(|policy| {
            let name = policy
                .name
                .map_or_else(String::new, |name| name.to_string());
            format!("{name}:{}", policy.maximum_retry_count.unwrap_or(0))
        }),
        shm_size: host.shm_size,
    }
}

/// Verifies the inspected policy against exactly what the adapter constructed.
///
/// Every check fails closed on a missing report. Safety-negative lists
/// (`cap_add`, binds, mounts, devices) treat an absent report as empty because
/// Docker omits empty lists in inspect output; a reported non-empty list is a
/// mismatch.
pub(crate) fn verify(spec: &CreateSpec, host: &HostReport) -> Result<(), PolicyMismatch> {
    expect("runtime", &spec.runtime, host.runtime.as_ref())?;
    expect("memory", &spec.memory, host.memory_bytes.as_ref())?;
    expect("memory_swap", &spec.memory, host.memory_swap_bytes.as_ref())?;
    expect("nano_cpus", &spec.nano_cpus, host.nano_cpus.as_ref())?;
    expect("pids_limit", &spec.pids, host.pids_limit.as_ref())?;
    expect("readonly_rootfs", &true, host.readonly_root.as_ref())?;
    expect_str("network_mode", NETWORK_MODE, host.network_mode.as_ref())?;
    verify_contains("cap_drop", "ALL", host.cap_drop.as_deref())?;
    verify_empty("cap_add", host.cap_add.as_deref())?;
    verify_contains("security_opt", SECURITY_OPT, host.security_opt.as_deref())?;
    verify_count("binds", host.bind_count)?;
    verify_count("mounts", host.mount_count)?;
    verify_count("devices", host.device_count)?;
    verify_tmpfs(spec, host.tmpfs.as_ref())?;
    expect("init", &true, host.init.as_ref())?;
    expect_str("ipc_mode", IPC_MODE, host.ipc_mode.as_ref())?;
    expect_str("cgroupns_mode", CGROUPNS_MODE, host.cgroupns_mode.as_ref())?;
    expect("oom_kill_disable", &false, host.oom_kill_disable.as_ref())?;
    expect("auto_remove", &false, host.auto_remove.as_ref())?;
    expect_str(
        "restart_policy",
        RESTART_POLICY,
        host.restart_policy.as_ref(),
    )?;
    expect("shm_size", &SHM_SIZE, host.shm_size.as_ref())?;
    Ok(())
}

fn mismatch(
    field: &'static str,
    expected: impl Into<String>,
    actual: impl Into<String>,
) -> PolicyMismatch {
    PolicyMismatch {
        field,
        expected: expected.into(),
        actual: actual.into(),
    }
}

fn expect<T: PartialEq + std::fmt::Display>(
    field: &'static str,
    expected: &T,
    actual: Option<&T>,
) -> Result<(), PolicyMismatch> {
    match actual {
        Some(value) if value == expected => Ok(()),
        Some(value) => Err(mismatch(field, expected.to_string(), value.to_string())),
        None => Err(mismatch(field, expected.to_string(), "not reported")),
    }
}

fn expect_str(
    field: &'static str,
    expected: &str,
    actual: Option<&String>,
) -> Result<(), PolicyMismatch> {
    match actual {
        Some(value) if value.as_str() == expected => Ok(()),
        Some(value) => Err(mismatch(field, expected, value.clone())),
        None => Err(mismatch(field, expected, "not reported")),
    }
}

fn verify_contains(
    field: &'static str,
    expected: &str,
    actual: Option<&[String]>,
) -> Result<(), PolicyMismatch> {
    match actual {
        Some(values)
            if values
                .iter()
                .any(|value| value.eq_ignore_ascii_case(expected)) =>
        {
            Ok(())
        }
        Some(values) => Err(mismatch(field, expected, format!("{values:?}"))),
        None => Err(mismatch(field, expected, "not reported")),
    }
}

fn verify_empty(field: &'static str, actual: Option<&[String]>) -> Result<(), PolicyMismatch> {
    match actual {
        Some([]) | None => Ok(()),
        Some(values) => Err(mismatch(field, "empty", format!("{values:?}"))),
    }
}

fn verify_count(field: &'static str, actual: Option<usize>) -> Result<(), PolicyMismatch> {
    match actual {
        None | Some(0) => Ok(()),
        Some(count) => Err(mismatch(field, "empty", count.to_string())),
    }
}

fn verify_tmpfs(
    spec: &CreateSpec,
    actual: Option<&BTreeMap<String, String>>,
) -> Result<(), PolicyMismatch> {
    let Some(tmpfs) = actual else {
        return Err(mismatch("tmpfs", expected_tmpfs(spec), "not reported"));
    };
    if tmpfs.len() != 1 || !tmpfs.contains_key(&spec.workspace_path) {
        let actual = tmpfs
            .iter()
            .map(|(path, options)| format!("{path}={options}"))
            .collect::<Vec<_>>()
            .join(",");
        return Err(mismatch(
            "tmpfs",
            expected_tmpfs(spec),
            if actual.is_empty() {
                "empty".to_string()
            } else {
                actual
            },
        ));
    }
    let options = &tmpfs[&spec.workspace_path];
    for required in required_tmpfs_options(spec) {
        if !options.split(',').any(|option| option.trim() == required) {
            return Err(mismatch(
                "tmpfs",
                expected_tmpfs(spec),
                format!("{}={options}", spec.workspace_path),
            ));
        }
    }
    Ok(())
}

fn required_tmpfs_options(spec: &CreateSpec) -> [String; 5] {
    [
        "nosuid".to_string(),
        "nodev".to_string(),
        format!("uid={}", spec.guest_uid),
        format!("gid={}", spec.guest_gid),
        format!("size={}", spec.workspace_bytes),
    ]
}

fn expected_tmpfs(spec: &CreateSpec) -> String {
    format!(
        "{}={}",
        spec.workspace_path,
        required_tmpfs_options(spec).join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    fn spec() -> CreateSpec {
        CreateSpec {
            image: "python:3.12-slim".to_string(),
            runtime: "runc".to_string(),
            memory: 64 * 1024 * 1024,
            nano_cpus: 100_000_000,
            pids: 32,
            workspace_path: "/workspace".to_string(),
            workspace_bytes: 8 * 1024 * 1024,
            guest_uid: 65_534,
            guest_gid: 65_534,
            owner_label: "owner".to_string(),
        }
    }

    fn inspected() -> HostReport {
        let spec = spec();
        let mut tmpfs = HashMap::new();
        tmpfs.insert(
            spec.workspace_path.clone(),
            format!(
                "rw,nosuid,nodev,uid={},gid={},mode=0755,size={}",
                spec.guest_uid, spec.guest_gid, spec.workspace_bytes
            ),
        );
        report_from(Some(bollard::models::HostConfig {
            runtime: Some(spec.runtime.clone()),
            memory: Some(spec.memory),
            memory_swap: Some(spec.memory),
            nano_cpus: Some(spec.nano_cpus),
            pids_limit: Some(spec.pids),
            readonly_rootfs: Some(true),
            network_mode: Some(NETWORK_MODE.to_string()),
            cap_drop: Some(vec!["ALL".to_string()]),
            cap_add: Some(Vec::new()),
            security_opt: Some(vec![SECURITY_OPT.to_string()]),
            binds: Some(Vec::new()),
            mounts: Some(Vec::new()),
            devices: Some(Vec::new()),
            tmpfs: Some(tmpfs),
            init: Some(true),
            ipc_mode: Some(IPC_MODE.to_string()),
            cgroupns_mode: Some(bollard::models::HostConfigCgroupnsModeEnum::PRIVATE),
            oom_kill_disable: Some(false),
            auto_remove: Some(false),
            restart_policy: Some(bollard::models::RestartPolicy {
                name: Some(bollard::models::RestartPolicyNameEnum::NO),
                maximum_retry_count: Some(0),
            }),
            shm_size: Some(SHM_SIZE),
            ..Default::default()
        }))
    }

    #[test]
    fn matching_report_verifies() {
        assert_eq!(verify(&spec(), &inspected()), Ok(()));
    }

    #[test]
    fn create_body_is_exactly_what_verify_accepts() {
        let spec = spec();
        let body = create_body("sandbox-a", &spec);
        assert_eq!(body.image.as_deref(), Some(spec.image.as_str()));
        let user = format!("{}:{}", spec.guest_uid, spec.guest_gid);
        assert_eq!(body.user.as_deref(), Some(user.as_str()));
        assert_eq!(
            body.working_dir.as_deref(),
            Some(spec.workspace_path.as_str())
        );
        assert_eq!(body.network_disabled, Some(true));
        let labels = body.labels.as_ref().expect("labels");
        assert_eq!(
            labels.get(OWNER_LABEL_KEY).map(String::as_str),
            Some(spec.owner_label.as_str())
        );
        assert_eq!(
            labels.get("circus.sandbox.name").map(String::as_str),
            Some("sandbox-a")
        );
        let report = report_from(body.host_config);
        assert_eq!(verify(&spec, &report), Ok(()));
    }

    #[test]
    fn policy_mismatch_error_keeps_typed_policy() {
        let mismatch = PolicyMismatch {
            field: "network_mode",
            expected: "none".to_string(),
            actual: "bridge".to_string(),
        };
        let err = mismatch.clone().error();
        assert!(err.message.contains("network_mode"), "{err:?}");
        assert_eq!(err.policy.as_deref(), Some(&mismatch));
        assert!(!err.not_found);
        assert!(!err.create_pending);
    }

    #[test]
    fn empty_reported_tmpfs_is_a_mismatch() {
        let mut host = inspected();
        host.tmpfs = Some(BTreeMap::new());
        let mismatch = verify(&spec(), &host).unwrap_err();
        assert_eq!(mismatch.field, "tmpfs");
        assert_eq!(mismatch.actual, "empty");
    }

    #[test]
    fn absent_scalar_fields_fail_closed() {
        let mut host = inspected();
        host.memory_bytes = None;
        assert_eq!(verify(&spec(), &host).unwrap_err().actual, "not reported");

        let mut host = inspected();
        host.pids_limit = None;
        assert_eq!(verify(&spec(), &host).unwrap_err().field, "pids_limit");

        let mut host = inspected();
        host.cap_drop = None;
        assert_eq!(verify(&spec(), &host).unwrap_err().field, "cap_drop");
    }

    #[test]
    fn memory_is_not_swap_and_each_field_is_attributed() {
        let spec = spec();
        let mut host = inspected();
        host.memory_swap_bytes = Some(128);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "memory_swap");

        let mut host = inspected();
        host.nano_cpus = Some(1);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "nano_cpus");

        let mut host = inspected();
        host.readonly_root = Some(false);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "readonly_rootfs");

        let mut host = inspected();
        host.network_mode = Some("bridge".to_string());
        assert_eq!(verify(&spec, &host).unwrap_err().field, "network_mode");

        let mut host = inspected();
        host.cap_drop = Some(vec!["NET_RAW".to_string()]);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "cap_drop");

        let mut host = inspected();
        host.cap_add = Some(vec!["SYS_ADMIN".to_string()]);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "cap_add");

        let mut host = inspected();
        host.security_opt = Some(Vec::new());
        assert_eq!(verify(&spec, &host).unwrap_err().field, "security_opt");

        let mut host = inspected();
        host.bind_count = Some(1);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "binds");

        let mut host = inspected();
        host.mount_count = Some(1);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "mounts");

        let mut host = inspected();
        host.device_count = Some(1);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "devices");

        let mut host = inspected();
        host.init = Some(false);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "init");

        let mut host = inspected();
        host.ipc_mode = Some("host".to_string());
        assert_eq!(verify(&spec, &host).unwrap_err().field, "ipc_mode");

        let mut host = inspected();
        host.cgroupns_mode = Some("host".to_string());
        assert_eq!(verify(&spec, &host).unwrap_err().field, "cgroupns_mode");

        let mut host = inspected();
        host.oom_kill_disable = Some(true);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "oom_kill_disable");

        let mut host = inspected();
        host.auto_remove = Some(true);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "auto_remove");

        let mut host = inspected();
        host.restart_policy = Some("always:0".to_string());
        assert_eq!(verify(&spec, &host).unwrap_err().field, "restart_policy");

        let mut host = inspected();
        host.shm_size = Some(1);
        assert_eq!(verify(&spec, &host).unwrap_err().field, "shm_size");

        let mut host = inspected();
        host.runtime = Some("runc-other".to_string());
        assert_eq!(verify(&spec, &host).unwrap_err().field, "runtime");
    }

    #[test]
    fn tmpfs_must_be_exactly_the_configured_workspace() {
        let spec = spec();
        let mut host = inspected();
        host.tmpfs
            .as_mut()
            .expect("baseline tmpfs")
            .insert("/other".to_string(), "rw".to_string());
        assert_eq!(verify(&spec, &host).unwrap_err().field, "tmpfs");

        let mut host = inspected();
        host.tmpfs.as_mut().unwrap().insert(
            "/workspace".to_string(),
            "rw,uid=65534,gid=65534,mode=0755,size=8388608".to_string(),
        );
        let mismatch = verify(&spec, &host).unwrap_err();
        assert_eq!(mismatch.field, "tmpfs");
        assert!(mismatch.expected.contains("nosuid"), "{mismatch:?}");

        let mut host = inspected();
        host.tmpfs = None;
        assert_eq!(verify(&spec, &host).unwrap_err().field, "tmpfs");
    }

    #[test]
    fn missing_host_report_fails_closed() {
        let host = report_from(None);
        let mismatch = verify(&spec(), &host).unwrap_err();
        assert_eq!(mismatch.field, "runtime");
        assert_eq!(mismatch.actual, "not reported");
    }
}

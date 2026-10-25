//! Pod construction, create-response verification, and observable reporting.
//!
//! Only values read back from the API server become [`HostReport`] evidence.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Capabilities as KubeCapabilities, Container, EmptyDirVolumeSource, Pod, PodSecurityContext,
    PodSpec, ResourceRequirements, SeccompProfile, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use super::CommonSpec;
use super::engine::{HostReport, OWNER_LABEL_KEY, PolicyMismatch};

/// Fixed container name; the Pod has exactly one container.
pub(crate) const CONTAINER: &str = "sandbox";
/// Hard backstop for a workload that was never destroyed. One day is far above
/// any configured lifecycle deadline and still reclaims leaked Pods.
pub(crate) const ACTIVE_DEADLINE_SECONDS: i64 = 86_400;
/// Bounded entrypoint; the guest helper and every exec require Python 3.
const ENTRYPOINT: &str = "import time; time.sleep(31536000)";
const NAME_LABEL: &str = "circus.sandbox.name";
const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

/// Builds the one owned Pod. No host path, host network, or ambient service
/// account token is requested.
pub(crate) fn build(
    name: &str,
    nonce: &str,
    spec: &CommonSpec,
    image: &str,
    service_account: Option<&str>,
) -> Pod {
    let mut labels = BTreeMap::new();
    labels.insert(OWNER_LABEL_KEY.to_string(), nonce.to_string());
    labels.insert(NAME_LABEL.to_string(), name.to_string());
    labels.insert(MANAGED_BY_LABEL.to_string(), "circus-sandbox".to_string());

    let seccomp = || {
        Some(SeccompProfile {
            type_: "RuntimeDefault".to_string(),
            localhost_profile: None,
        })
    };
    let mut limits = BTreeMap::new();
    limits.insert("memory".to_string(), Quantity(spec.memory.to_string()));
    limits.insert(
        "cpu".to_string(),
        Quantity(format!("{}m", millis(spec.nano_cpus))),
    );
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            active_deadline_seconds: Some(ACTIVE_DEADLINE_SECONDS),
            automount_service_account_token: Some(false),
            containers: vec![Container {
                name: CONTAINER.to_string(),
                image: Some(image.to_string()),
                image_pull_policy: Some("IfNotPresent".to_string()),
                command: Some(vec![
                    "python3".to_string(),
                    "-I".to_string(),
                    "-B".to_string(),
                    "-c".to_string(),
                    ENTRYPOINT.to_string(),
                ]),
                args: Some(Vec::new()),
                env: Some(Vec::new()),
                working_dir: Some(spec.workspace_path.clone()),
                resources: Some(ResourceRequirements {
                    limits: Some(limits.clone()),
                    requests: Some(limits),
                    claims: None,
                }),
                security_context: Some(SecurityContext {
                    allow_privilege_escalation: Some(false),
                    capabilities: Some(KubeCapabilities {
                        add: Some(Vec::new()),
                        drop: Some(vec!["ALL".to_string()]),
                    }),
                    privileged: Some(false),
                    read_only_root_filesystem: Some(true),
                    run_as_group: Some(group(spec.guest_gid)),
                    run_as_non_root: Some(true),
                    run_as_user: Some(user(spec.guest_uid)),
                    seccomp_profile: seccomp(),
                    ..SecurityContext::default()
                }),
                volume_mounts: Some(vec![VolumeMount {
                    name: "workspace".to_string(),
                    mount_path: spec.workspace_path.clone(),
                    read_only: Some(false),
                    ..VolumeMount::default()
                }]),
                ..Container::default()
            }],
            enable_service_links: Some(false),
            host_ipc: Some(false),
            host_network: Some(false),
            host_pid: Some(false),
            restart_policy: Some("Never".to_string()),
            security_context: Some(PodSecurityContext {
                fs_group: Some(group(spec.guest_gid)),
                fs_group_change_policy: Some("OnRootMismatch".to_string()),
                run_as_group: Some(group(spec.guest_gid)),
                run_as_non_root: Some(true),
                run_as_user: Some(user(spec.guest_uid)),
                seccomp_profile: seccomp(),
                ..PodSecurityContext::default()
            }),
            service_account_name: service_account.map(str::to_string),
            share_process_namespace: Some(false),
            termination_grace_period_seconds: Some(0),
            volumes: Some(vec![Volume {
                name: "workspace".to_string(),
                empty_dir: Some(EmptyDirVolumeSource {
                    medium: None,
                    size_limit: Some(Quantity(spec.workspace_bytes.to_string())),
                }),
                ..Volume::default()
            }]),
            ..PodSpec::default()
        }),
        status: None,
    }
}

fn user(uid: u32) -> i64 {
    i64::from(uid)
}

fn group(gid: u32) -> i64 {
    i64::from(gid)
}

fn millis(nano_cpus: i64) -> i64 {
    (nano_cpus.max(0) / 1_000_000).max(1)
}

/// Verifies the created Pod still carries the requested ownership and policy.
///
/// The adapter fails creation on any difference so a mutated or admitted Pod is
/// destroyed instead of being reported as enforced.
pub(crate) fn verify(
    pod: &Pod,
    name: &str,
    nonce: &str,
    spec: &CommonSpec,
    image: &str,
) -> Result<(), PolicyMismatch> {
    let metadata = &pod.metadata;
    expect("name", name, metadata.name.as_deref())?;
    let labels = metadata.labels.as_ref();
    expect(
        "owner label",
        nonce,
        labels
            .and_then(|labels| labels.get(OWNER_LABEL_KEY))
            .map(String::as_str),
    )?;
    expect(
        "name label",
        name,
        labels
            .and_then(|labels| labels.get(NAME_LABEL))
            .map(String::as_str),
    )?;
    let pod_spec = pod
        .spec
        .as_ref()
        .ok_or_else(|| mismatch("spec", "present", "absent"))?;
    expect("restartPolicy", "Never", pod_spec.restart_policy.as_deref())?;
    expect(
        "activeDeadlineSeconds",
        &ACTIVE_DEADLINE_SECONDS.to_string(),
        pod_spec
            .active_deadline_seconds
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
    )?;
    let security = pod_spec
        .security_context
        .as_ref()
        .ok_or_else(|| mismatch("securityContext", "present", "absent"))?;
    expect(
        "runAsNonRoot",
        "true",
        security
            .run_as_non_root
            .map(|value| value.to_string())
            .as_deref(),
    )?;
    expect(
        "seccompProfile",
        "RuntimeDefault",
        security
            .seccomp_profile
            .as_ref()
            .map(|profile| profile.type_.as_str()),
    )?;
    let containers = &pod_spec.containers;
    if containers.len() != 1 {
        return Err(mismatch(
            "container count",
            "1",
            &containers.len().to_string(),
        ));
    }
    let container = &containers[0];
    expect("image", image, container.image.as_deref())?;
    expect(
        "imagePullPolicy",
        "IfNotPresent",
        container.image_pull_policy.as_deref(),
    )?;
    expect(
        "workingDir",
        &spec.workspace_path,
        container.working_dir.as_deref(),
    )?;
    let security = container
        .security_context
        .as_ref()
        .ok_or_else(|| mismatch("container securityContext", "present", "absent"))?;
    expect(
        "allowPrivilegeEscalation",
        "false",
        security
            .allow_privilege_escalation
            .map(|value| value.to_string())
            .as_deref(),
    )?;
    expect(
        "readOnlyRootFilesystem",
        "true",
        security
            .read_only_root_filesystem
            .map(|value| value.to_string())
            .as_deref(),
    )?;
    expect(
        "container runAsNonRoot",
        "true",
        security
            .run_as_non_root
            .map(|value| value.to_string())
            .as_deref(),
    )?;
    expect(
        "capability drop",
        "ALL",
        security
            .capabilities
            .as_ref()
            .and_then(|caps| caps.drop.as_ref())
            .and_then(|drop| drop.first())
            .map(String::as_str),
    )?;
    let volumes = pod_spec.volumes.as_deref().unwrap_or_default();
    for volume in volumes {
        if volume.host_path.is_some() {
            return Err(mismatch("hostPath volume", "none", &volume.name));
        }
    }
    let workspace = volumes
        .iter()
        .find(|volume| volume.name == "workspace")
        .ok_or_else(|| mismatch("workspace volume", "emptyDir", "absent"))?;
    let empty = workspace
        .empty_dir
        .as_ref()
        .ok_or_else(|| mismatch("workspace volume", "emptyDir", "other"))?;
    expect(
        "workspace sizeLimit",
        &spec.workspace_bytes.to_string(),
        empty
            .size_limit
            .as_ref()
            .map(|quantity| quantity.0.as_str()),
    )?;
    Ok(())
}

/// Reports only what the Pod spec contains. Unverifiable fields stay absent.
pub(crate) fn report(pod: &Pod) -> HostReport {
    let Some(pod_spec) = pod.spec.as_ref() else {
        return HostReport::default();
    };
    let container = pod_spec.containers.first();
    let limits = container
        .and_then(|container| container.resources.as_ref())
        .and_then(|resources| resources.limits.as_ref());
    let mut security_opt = Vec::new();
    if let Some(security) = container.and_then(|container| container.security_context.as_ref()) {
        if security.allow_privilege_escalation == Some(false) {
            security_opt.push("allow-privilege-escalation:false".to_string());
        }
        if security
            .seccomp_profile
            .as_ref()
            .is_some_and(|profile| profile.type_ == "RuntimeDefault")
        {
            security_opt.push("seccomp:RuntimeDefault".to_string());
        }
    }
    let host_volumes = pod_spec
        .volumes
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|volume| volume.host_path.is_some())
        .count();
    HostReport {
        memory_bytes: limits
            .and_then(|limits| limits.get("memory"))
            .and_then(|quantity| parse_memory(&quantity.0)),
        nano_cpus: limits
            .and_then(|limits| limits.get("cpu"))
            .and_then(|quantity| parse_cpu(&quantity.0)),
        pids_limit: None,
        readonly_root: container
            .and_then(|container| container.security_context.as_ref())
            .and_then(|security| security.read_only_root_filesystem),
        network_mode: None,
        cap_drop: container
            .and_then(|container| container.security_context.as_ref())
            .and_then(|security| security.capabilities.as_ref())
            .and_then(|capabilities| capabilities.drop.clone()),
        security_opt: if security_opt.is_empty() {
            None
        } else {
            Some(security_opt)
        },
        bind_count: pod_spec.volumes.as_ref().map(|_| host_volumes),
        mount_count: container
            .and_then(|container| container.volume_mounts.as_ref())
            .map(Vec::len),
        ..HostReport::default()
    }
}

/// A Ready condition, or a running container, satisfies startup.
pub(crate) fn started(pod: &Pod) -> bool {
    let status = pod.status.as_ref();
    let ready = status
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|condition| condition.type_ == "Ready" && condition.status == "True")
        });
    let running = status
        .and_then(|status| status.container_statuses.as_ref())
        .is_some_and(|statuses| {
            statuses.iter().any(|status| {
                status
                    .state
                    .as_ref()
                    .and_then(|state| state.running.as_ref())
                    .is_some()
            })
        });
    ready || running
}

/// A terminal Pod state or a container waiting on an unrecoverable setup error.
pub(crate) fn failure(pod: &Pod) -> Option<String> {
    let status = pod.status.as_ref()?;
    if let Some(reason) = status.reason.as_deref()
        && (reason == "Evicted" || reason == "DeadlineExceeded")
    {
        return Some(format!("pod {reason}"));
    }
    if status.phase.as_deref() == Some("Failed") {
        return Some(format!(
            "pod failed: {}",
            status.message.as_deref().unwrap_or("no message")
        ));
    }
    for container in status.container_statuses.as_deref().unwrap_or_default() {
        if let Some(waiting) = container
            .state
            .as_ref()
            .and_then(|state| state.waiting.as_ref())
        {
            let reason = waiting.reason.as_deref().unwrap_or_default();
            if matches!(
                reason,
                "ErrImagePull"
                    | "ImagePullBackOff"
                    | "InvalidImageName"
                    | "CreateContainerConfigError"
                    | "CreateContainerError"
                    | "RunContainerError"
            ) {
                return Some(format!(
                    "container cannot start ({reason}): {}",
                    waiting.message.as_deref().unwrap_or("no message")
                ));
            }
        }
        if let Some(terminated) = container
            .state
            .as_ref()
            .and_then(|state| state.terminated.as_ref())
            && terminated.exit_code != 0
        {
            return Some(format!(
                "container exited with {}: {}",
                terminated.exit_code,
                terminated.reason.as_deref().unwrap_or("no reason")
            ));
        }
    }
    None
}

/// Parses a memory quantity from a Pod spec. Binary and decimal suffixes are
/// supported; fractional values are rejected rather than rounded silently.
fn parse_memory(value: &str) -> Option<i64> {
    let (digits, multiplier) = [
        ("Ki", 1024_i64),
        ("Mi", 1024 * 1024),
        ("Gi", 1024 * 1024 * 1024),
        ("Ti", 1024_i64.pow(4)),
        ("K", 1_000),
        ("M", 1_000_000),
        ("G", 1_000_000_000),
        ("T", 1_000_000_000_000),
    ]
    .into_iter()
    .find_map(|(suffix, multiplier)| {
        value
            .strip_suffix(suffix)
            .map(|digits| (digits, multiplier))
    })
    .unwrap_or((value, 1));
    digits
        .parse::<i64>()
        .ok()
        .and_then(|digits| digits.checked_mul(multiplier))
}

/// Parses a CPU quantity in millicores or whole cores into nanocpus.
fn parse_cpu(value: &str) -> Option<i64> {
    if let Some(millis) = value.strip_suffix('m') {
        return millis
            .parse::<i64>()
            .ok()
            .and_then(|millis| millis.checked_mul(1_000_000));
    }
    value
        .parse::<i64>()
        .ok()
        .and_then(|cores| cores.checked_mul(1_000_000_000))
}

fn expect(field: &'static str, expected: &str, actual: Option<&str>) -> Result<(), PolicyMismatch> {
    match actual {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(mismatch(field, expected, actual)),
        None => Err(mismatch(field, expected, "not reported")),
    }
}

fn mismatch(field: &'static str, expected: &str, actual: &str) -> PolicyMismatch {
    PolicyMismatch {
        field,
        expected: expected.to_string(),
        actual: actual.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CommonSpec {
        CommonSpec {
            memory: 128 * 1024 * 1024,
            nano_cpus: 500_000_000,
            pids: 64,
            workspace_path: "/workspace".to_string(),
            workspace_bytes: 16 * 1024 * 1024,
            guest_uid: 1000,
            guest_gid: 1000,
        }
    }

    #[test]
    fn built_pod_is_recognized_by_verification_and_report() {
        let spec = spec();
        let owner = uuid::Uuid::new_v4().to_string();
        let pod = build(
            "circus-sandbox-test",
            &owner,
            &spec,
            "python:3.12-slim",
            None,
        );
        verify(
            &pod,
            "circus-sandbox-test",
            &owner,
            &spec,
            "python:3.12-slim",
        )
        .expect("constructed pod verifies");
        let report = report(&pod);
        assert_eq!(report.memory_bytes, Some(128 * 1024 * 1024));
        assert_eq!(report.nano_cpus, Some(500_000_000));
        assert_eq!(report.readonly_root, Some(true));
        assert_eq!(report.cap_drop, Some(vec!["ALL".to_string()]));
        assert_eq!(report.bind_count, Some(0));
        assert_eq!(report.pids_limit, None);
    }

    #[test]
    fn verification_rejects_foreign_labels_and_host_paths() {
        let spec = spec();
        let owner = uuid::Uuid::new_v4().to_string();
        let pod = build(
            "circus-sandbox-test",
            &owner,
            &spec,
            "python:3.12-slim",
            None,
        );
        let foreign = uuid::Uuid::new_v4().to_string();
        assert!(
            verify(
                &pod,
                "circus-sandbox-test",
                &foreign,
                &spec,
                "python:3.12-slim"
            )
            .is_err()
        );
        let mut mutated = pod;
        mutated.spec.as_mut().expect("spec").volumes = Some(vec![Volume {
            name: "host".to_string(),
            host_path: Some(k8s_openapi::api::core::v1::HostPathVolumeSource {
                path: "/".to_string(),
                type_: None,
            }),
            ..Volume::default()
        }]);
        assert!(
            verify(
                &mutated,
                "circus-sandbox-test",
                &owner,
                &spec,
                "python:3.12-slim"
            )
            .is_err()
        );
    }

    #[test]
    fn quantity_parsing_stays_integral() {
        assert_eq!(parse_memory("128Mi"), Some(128 * 1024 * 1024));
        assert_eq!(parse_memory("134217728"), Some(134_217_728));
        assert_eq!(parse_memory("1.5Gi"), None);
        assert_eq!(parse_cpu("500m"), Some(500_000_000));
        assert_eq!(parse_cpu("2"), Some(2_000_000_000));
    }
}

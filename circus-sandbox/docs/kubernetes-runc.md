# Kubernetes workload (runc) backend

This document records the B02 Kubernetes workload adapter, the S03-kr
workspace snapshot slice, and the interactive-process slice: what it actually
does, the versions and commands used for the recorded run, least-privilege
RBAC, security settings, cleanup/ownership, and what remains unverified. It does
not claim true pause/resume or a VM runtime.

## Scope

Implemented:

- `BackendConfig::Kubernetes(KubernetesBackendConfig)` behind the optional
  `kubernetes` crate feature.
- Explicit kubeconfig loading (never `KUBECONFIG`, `$HOME/.kube/config`, or
  in-cluster credentials), namespace-scoped API access, API preparation with a
  server-side dry run, Pod create/start, argv exec, bounded guest file transport,
  inspected host reporting, ownership-verified deletion, and typed unsupported
  operations.
- Interactive `spawn_process` sessions over `pods/exec`: an owned stdin writer, a
  merged ordered stdout/stderr stream, and an exit future that joins the
  transport before mapping the status. The capability report sets
  `spawn_process: true`; lifecycle still owns the one-process limit, timeout,
  output bounds, cancellation, and Pod cleanup.
- Workspace file snapshot capture/restore over the same `pods/exec` guest file
  protocol used by Docker and containerd. The capability report sets
  `workspace_snapshot: true`.
- Backend-neutral `SandboxPool` leases: warm capacity, single-use Pods, and
  release/TTL/shutdown revocation through the shared lifecycle. The pool never
  resets or reuses a Pod.
- `tests/kubernetes_workload.rs`, `tests/kubernetes_snapshot.rs`, and
  `tests/kubernetes_pool.rs`, ignored unless `CIRCUS_KUBERNETES=1`; a real k3s
  job in `.github/workflows/sandbox-kubernetes.yml`.

Not implemented:

- Pause/resume (B03 provider). `pause_resume` stays false.
- Process or guest-memory checkpointing. Snapshots copy regular workspace files
  and directories only.
- TTY allocation, terminal resize, process-group signals, and a detached
  session/recovery protocol.
- pids limits, RuntimeClass selection, image pull credentials, NetworkPolicy
  creation, or KVM/gVisor/Kata runtimes.

## Versions used for the recorded run

Recorded in this WSL workstation against the running k3s cluster:

| Component          | Value                                                                                                                        |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------- |
| k3s                | `v1.36.5+k3s1` (`3dd98cc5`)                                                                                                  |
| Kubernetes node    | `v1.36.5+k3s1`                                                                                                               |
| containerd         | `2.3.4-k3s1.36` (reported by the node)                                                                                       |
| Node OS / kernel   | Ubuntu 24.04.4 LTS, `6.6.87.2-microsoft-standard-WSL2`                                                                       |
| Rust               | `1.99.1`, edition 2024                                                                                                       |
| kube / k8s-openapi | `4.2.0` / `0.28.0` (`v1_36`)                                                                                                 |
| Image              | `docker.io/library/python:3.12-slim`, index digest `sha256:a0f57064cbdcd6285fa45f51237ab0b35ea8ebaed06a1f5973e89e19dfb86704` |

The node was already Ready and the image was already present in k3s
containerd. The backend's `prepare` never pulls; kubelet pulls a missing image
with `imagePullPolicy: IfNotPresent`.

```text
$ /usr/local/bin/k3s kubectl get nodes -o wide
NAME              STATUS   ROLES           AGE   VERSION        INTERNAL-IP   CONTAINER-RUNTIME
desktop-8nqa5gc   Ready    control-plane   125m  v1.36.5+k3s1  100.100.4.15  containerd://2.3.4-k3s1.36
```

```text
$ /usr/local/bin/k3s ctr -n k8s.io images list | grep python:3.12-slim
docker.io/library/python:3.12-slim  application/vnd.oci.image.index.v1+json  sha256:a0f57064cbdcd6285fa45f51237ab0b35ea8ebaed06a1f5973e89e19dfb86704  41.3 MiB
```

## Descriptor and creation

```rust
use circus_sandbox::{BackendConfig, KubernetesBackendConfig};

let backend = BackendConfig::Kubernetes(
    KubernetesBackendConfig::new(
        "/etc/rancher/k3s/k3s.yaml",
        "default",
        "docker.io/library/python:3.12-slim",
    )
    .with_context("default")                 // optional
    .with_service_account("sandbox-runner"), // optional
);
```

Validation happens before any API allocation:

| Field             | Rule                                                              |
| ----------------- | ----------------------------------------------------------------- |
| `kubeconfig`      | absolute path (Unix or host-absolute)                             |
| `context`         | optional, nonempty, at most 253 characters, no control characters |
| `namespace`       | DNS-1123 label                                                    |
| `service_account` | optional DNS-1123 subdomain                                       |
| `image`           | nonempty, lowercase OCI/Docker reference (tag/digest accepted)    |
| guest uid         | non-zero because the Pod sets `runAsNonRoot`                      |

Failures are `SandboxError::BackendConfig(BackendConfigError)` with
`InvalidKubeconfigPath`, `InvalidKubernetesNamespace`, `InvalidContextName`,
`InvalidServiceAccount`, `InvalidImageReference`, `ImageRequired`, or
`NonRootGuestRequired`. The descriptor is plain data; `kube`/`k8s-openapi`
types never appear in the public API, and the default build does not compile
those crates.

`Sandbox::create` performs `prepare` before allocating:

1. Load the explicit kubeconfig and resolve the requested context.
2. Override the default namespace with the descriptor's namespace.
3. List Pods in that namespace (`limit=1`) to prove reachability and RBAC read.
4. Create the full Pod spec with `dryRun=All` under a generated name. This runs
   schema/admission validation and proves `pods create` without persisting.
5. Return the owned Pod. Readiness is awaited by `start`, not by `create`.

An unreachable API, unreadable kubeconfig, unknown context, or forbidden
namespace is a typed `SandboxError::Backend { message }` and leaves no Pod. A
well-formed but unresolvable image cannot be detected without a pull; it fails
during `start` (kubelet `ErrImagePull`/`ImagePullBackOff`) and lifecycle removes
the Pod. The suite asserts this with `registry.invalid/...`.

## Pod security settings

One Pod per sandbox, one container named `sandbox`:

- `metadata.name = circus-sandbox-<uuid>`; labels
  `circus.sandbox.owner=<nonce>`, `circus.sandbox.name=<name>`,
  `app.kubernetes.io/managed-by=circus-sandbox`.
- `restartPolicy: Never`, `activeDeadlineSeconds: 86400` as a hard backstop.
- Entrypoint `python3 -I -B -c 'import time; time.sleep(31536000)'`; Python 3 is
  required for the entrypoint, exec trampoline, and guest file helper.
- `securityContext` (Pod and container): `runAsNonRoot: true`, explicit
  `runAsUser`/`runAsGroup` from `IsolationConfig`, `fsGroup` with
  `OnRootMismatch`, `seccompProfile: RuntimeDefault`,
  `allowPrivilegeEscalation: false`, `readOnlyRootFilesystem: true`,
  `privileged: false`, capabilities `drop: ["ALL"]` and `add: []`.
- `serviceAccountName` only when configured; `automountServiceAccountToken:
false`; `enableServiceLinks: false`; `hostNetwork`/`hostPID`/`hostIPC` false;
  `shareProcessNamespace: false`; `terminationGracePeriodSeconds: 0`.
- Workspace is an `emptyDir` with `sizeLimit` from `ResourceLimits`, mounted at
  the configured guest path. There are no `hostPath` volumes and no host mounts.
- Memory and CPU `limits` and `requests` are copied from `ResourceLimits`.
  pids limits do not exist in a Pod spec for a normal container; the adapter
  deliberately reports `pids_limit: None` instead of pretending.
- `imagePullPolicy: IfNotPresent`; `create_owned` verifies that the API server
  preserved the owner labels, image, restart policy, deadline, security fields,
  workspace volume, and the absence of `hostPath` volumes. A mismatch destroys
  the Pod through the normal cleanup path.

`emptyDir.medium` is left unset, so the node chooses its storage. It is not
reported as tmpfs. Network isolation is not implemented by this adapter: pods
use the cluster CNI, and no NetworkPolicy is created. `hostNetwork: false` only
means the pod does not join the host network namespace.

## Exec, files, and status

- `exec` uses `pods/exec` with argv only. Kubernetes has no per-exec working
  directory or user field, so a small Python trampoline (`os.chdir`, then
  `os.execvp`) changes to the requested guest directory. `/` uses the command
  unchanged. The container's security context already fixes the uid/gid; the
  `user` argument is therefore inert.
- stdout and stderr are drained concurrently through the shared
  `OutputCollector`. The request's byte bound is a single aggregate budget;
  exceeding it is `SandboxError::OutputLimit` and lifecycle removes the Pod.
- `upload`, `download_file`, `remove_file`, and `list_workspace` run the same
  `guest_transport` argv and `guest_files.py` exit-code mapping as Docker and
  containerd. Staged files are streamed in 64 KiB chunks and bounded by the
  workspace per-file and aggregate limits; the manifest format is unchanged.
- `status().enforcement` reports only fields read back from the Pod spec:
  memory limit, CPU limit, read-only root, capability drop list, observed
  security options (`allow-privilege-escalation:false`,
  `seccomp:RuntimeDefault`), host-path bind count, and mount count. `runtime`,
  `network_mode`, `pids_limit`, and `tmpfs_workspace` stay `None`.

## Interactive processes

`Sandbox::spawn_process(ProcessRequest)` attaches through `pods/exec` with
`stdin/stdout/stderr: true`, `tty: false`, the Pod's single container (no
container override), and the same 64 KiB attach buffers as one-shot exec. The
lifecycle layer is unchanged: one process per sandbox, a lifetime timeout,
per-stream output bounds, cancellation, and cleanup remain its responsibility.
A second process or one-shot `exec` returns `SandboxError::ProcessActive` while
a session is attached. A normal exit (including a nonzero code) retains the Pod
and workspace. Timeout, output overflow, transport failure, session drop, or
`cancel` destroys the owned Pod and reports the observed `CleanupReport`; the
process transport never deletes a Pod.

Because `pods/exec` has no environment, user, or working-directory option, the
command runs through a positional Python trampoline:

```text
python3 -I -B -c <trampoline> -- <absolute-workdir> <override-count> <KEY=VALUE>... <user-argv>...
```

The trampoline changes to the absolute workdir, merges the explicit overrides
over the image environment (split at the first `=`, so values may contain `=`),
and `execvpe`s the user command. It is not a shell and never evaluates the user
argv as Python source: every value is a separate argv element after the
standalone `--`. The Pod already runs as the sandbox guest uid/gid, so no setuid
is attempted, and the `user` field is inert. Environment overrides are carried in
the exec request and in the trampoline's argument list while it starts; after
`exec` they are in the guest environment, readable by same-uid processes in the
sandbox like any image environment variable. This adapter does not log argv or
environment values. Startup failures use exit `125` for a chdir error and `127`
for an exec error.

Transport ownership and ordering:

- Output is a single ordered `ProcessChunk` stream built with `stream::unfold`
  over the two readers. Per-stream order is preserved; interleaving between
  stdout and stderr is the transport's order. No detached reader task is
  spawned, so dropping the stream stops reads.
- The exit future owns `AttachedProcess`. It is awaited only after output ends,
  joins the kube message loop first, and then maps the status channel through the
  same `exit_code` helper as one-shot exec. A missing status maps to `-1` and is
  never treated as success. Dropping the exit future drops `AttachedProcess`,
  whose `Drop` aborts the message loop.
- Dropping the stdin writer closes stdin (EOF) without stopping the Pod; the Pod
  remains lifecycle-owned.

## Workspace snapshots and pool leases

`Sandbox::capture_workspace_snapshot(budget, max_archive_bytes)` works on a Pod
in `Created` (host staging) or `Running`. A running Pod is read directly: it is
not paused, because Kubernetes has no pause operation and `pods/exec` cannot be
suspended. Capture calls `list_workspace` for one manifest, checks the entry
count, the configured per-file cap, and the budget's aggregate payload bound
before any file is read, then calls `download_file` per regular file and builds
the shared `CodeSnapshot::from_entries`. Directories, including empty ones, are
stored. A symlink, FIFO, device, socket, multiply-linked file, unportable name,
or manifest malformed at the byte bound fails closed as
`SandboxError::Snapshot(SnapshotError::InvalidArchive)`; an over-budget entry
count is `SnapshotError::EntryLimit`, an over-budget total is
`SnapshotError::ByteLimit`, and an over-cap file is
`SnapshotError::Workspace(WorkspaceError::LimitExceeded)`. Domain failures leave
the Pod running and owned; transport failures use the same whole-instance
cleanup as Docker and containerd.

`Sandbox::restore_workspace_snapshot(Arc<CodeSnapshot>)` is valid only in
`Created`. It validates the snapshot against the **target's** per-file and
workspace payload bounds, replaces host staging, and the next `start` uploads
that staging into the new Pod. It never overwrites a running Pod's workspace.
The unsafe-node, entry, byte, and target-bound behavior is exercised by
`tests/kubernetes_snapshot.rs` against real Pods.

`SandboxPool` treats a Pod like any other single-use instance: `PoolConfig`
capacity is warmed by real `Sandbox::create`/`start` calls, every lease gets a
fresh Pod, and `release`, lease TTL, or `shutdown` destroys it and confirms
absence. `tests/kubernetes_pool.rs` checks warm capacity, marker isolation
between leases, `kubectl`-confirmed Pod absence after release, acquire timeout,
TTL revocation of an in-flight exec, shutdown revocation, and a typed
preparation failure that leaves no Pod.

## Ownership and cleanup

The instance id is the Pod name. Deletion always starts with a live `GET`:

1. Pod absent → absence confirmed.
2. Pod present with the same `circus.sandbox.owner` nonce and
   `circus.sandbox.name` label → delete with `PropagationPolicy::Foreground`
   and `gracePeriodSeconds: 0`, then poll until `GET` returns 404. Five seconds
   without 404 is `StillPresent`; an inspection failure is `Unknown`.
3. Pod present with different labels → never delete. The sandbox retains
   ownership and reports `CleanupRequired` with `Ownership::Unknown`.
4. Any other inspection failure → retain ownership. A retained id alone never
   authorizes deletion because the Pod's labels are the only ownership proof.

`Sandbox::destroy` retries after the operator removes a foreign Pod; the suite
covers a foreign Pod with the same name and no owner label. `Drop` still asks
lifecycle to clean up, but only `destroy` confirms absence.

## Least-privilege RBAC

The backend only needs, in the target namespace:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  namespace: sandboxes
  name: circus-sandbox-workload
rules:
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["get", "list", "watch", "create", "delete"]
  - apiGroups: [""]
    resources: ["pods/exec"]
    verbs: ["create"]
  # Optional: not used by the adapter today, but useful for operator diagnosis.
  - apiGroups: [""]
    resources: ["pods/log"]
    verbs: ["get"]
```

Notes:

- `list` is used by `prepare`; `get` by start/status/recovery; `create` by both
  the dry run and the real Pod; `delete` by cleanup.
- `pods/exec` `create` is the exec subresource requirement. Snapshot capture,
  restore, and pool leases need no additional verbs: they reuse Pod create/get/
  delete and `pods/exec`.
- No deployments, replicasets, secrets, configmaps, nodes, namespaces, or
  service accounts are read. The kubeconfig user must be allowed to resolve its
  context and authenticate.
- The test suite's `kubectl` assertions use the same kubeconfig and need
  `pods get/delete` in that namespace; the CI job uses the k3s admin kubeconfig
  copied for the runner user.

## Recorded commands and results

In WSL (Rust 1.99.1, k3s already Ready), with the admin kubeconfig readable by
the test user:

```bash
export CIRCUS_KUBERNETES=1
export CIRCUS_KUBERNETES_KUBECONFIG=/etc/rancher/k3s/k3s.yaml
cargo test --locked --features kubernetes --test kubernetes_workload -- \
  --ignored --test-threads=1
cargo test --locked --features kubernetes --test kubernetes_snapshot -- \
  --ignored --test-threads=1
cargo test --locked --features kubernetes --test kubernetes_pool -- \
  --ignored --test-threads=1
```

Results recorded before the interactive-process slice (2026-10-25):

```text
running 7 tests
test kubernetes_absent_image_fails_start_without_leaking ... ok
test kubernetes_exec_output_limit_destroys_and_confirms_absence ... ok
test kubernetes_foreign_pod_with_same_name_is_never_deleted ... ok
test kubernetes_invalid_image_is_rejected_before_allocation ... ok
test kubernetes_linux_runc_lifecycle_exec_files_and_observables ... ok
test kubernetes_unreachable_api_is_typed_and_allocates_nothing ... ok
test kubernetes_unsupported_operations_are_typed ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.49s
```

The interactive-process slice adds two cases that are not covered by the run
above: `kubernetes_interactive_process_round_trip_and_nonzero_exit` (two-way
stdin/stdout, stderr, absolute workdir, `a=b` environment value, exit 7, retained
Pod and workspace) and `kubernetes_interactive_process_cancellation_cleans_up`
(`ProcessTermination::Cancelled`, `Ownership::ConfirmedAbsent`, destroyed sandbox,
Pod absence). Run the same suite command to record them on a real cluster; no
result is claimed here for them.

```text
running 4 tests
test kubernetes_captures_guest_workspace_and_restores_into_a_new_pod ... ok
test kubernetes_entry_byte_and_file_limits_fail_typed_and_preserve_the_pod ... ok
test kubernetes_restore_uses_target_bounds_and_preserves_existing_staging ... ok
test kubernetes_unsafe_guest_nodes_fail_closed_without_destroying_the_pod ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 34.76s
```

```text
running 6 tests
test kubernetes_pool_capacity_one_times_out_then_succeeds_after_release ... ok
test kubernetes_pool_failing_preparation_leaves_no_pod_and_a_typed_error ... ok
test kubernetes_pool_lease_ttl_revokes_in_flight_exec_and_refills ... ok
test kubernetes_pool_markers_do_not_cross_leases_and_release_destroys_the_pod ... ok
test kubernetes_pool_shutdown_revokes_leases_and_is_idempotent ... ok
test kubernetes_pool_warm_capacity_and_release_confirm_pod_absence ... ok

test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 53.22s
```

The suites create real Pods in `default`, run exec and file operations, capture
guest-written files into one Pod and restore them into another, check budget and
unsafe-node rejections, acquire and release pool leases, read `kubectl` state,
replace one owned Pod with a same-named foreign Pod to verify cleanup refusal,
and check absence with `kubectl get pod` after every destroy. The TTL test
interrupts an in-flight 60-second exec and then confirms the old Pod is absent.

Environment overrides:

| Variable                         | Default                                |
| -------------------------------- | -------------------------------------- |
| `CIRCUS_KUBERNETES`              | must be `1` to opt in                  |
| `CIRCUS_KUBERNETES_KUBECONFIG`   | `/etc/rancher/k3s/k3s.yaml`            |
| `CIRCUS_KUBERNETES_NAMESPACE`    | `default`                              |
| `CIRCUS_KUBERNETES_IMAGE`        | `docker.io/library/python:3.12-slim`   |
| `CIRCUS_KUBERNETES_ABSENT_IMAGE` | `registry.invalid/circus/absent:never` |

## What is not verified

- True pause/resume: no node-runtime or workload-provider pause exists. Pod
  recreation is not pause. `pause`/`resume` return
  `SandboxError::UnsupportedCapability { capability: "pause and resume" }`.
- Workspace snapshot scope: only regular files and directories are copied.
  There is no process, memory, or guest-filesystem checkpoint, no archive
  serialization, and no capture while a Pod is being deleted. Capture reads a
  running Pod live and fails closed if the manifest and payload disagree.
- Pool claims are per-Pod leases, not warm process/memory images: every lease
  still pulls nothing (kubelet uses `IfNotPresent`) but does start a fresh Pod,
  and no Pod is reused after `release`, TTL expiry, or `shutdown`.
- Interactive process scope: no TTY, terminal resize, process-group signal, or
  detached recovery exists. Cancellation destroys the whole Pod rather than
  signalling one guest process. Environment overrides and the working directory
  are applied by the argv trampoline because `pods/exec` has no such fields, so
  override values are visible to same-Pod processes through the guest process
  list; they are never logged by this adapter. Exit status depends on the API
  server's exec status channel; an absent status maps to `-1`, matching one-shot
  `exec`, and is never treated as success. Stdin EOF relies on the exec
  stream-close channel negotiated by kube 4.2; if an API server does not support
  it, the client closes the websocket instead, which can drop trailing output.
- pids limits, RuntimeClass selection, gVisor, Kata, Firecracker/KVM, registry
  authentication, image pull policy changes, multi-node scheduling behavior, and
  CNI/NetworkPolicy isolation.
- Node-level cgroup readback. `enforcement` reports the Pod spec, not the
  kubelet's effective cgroup values.
- Tenant isolation across namespaces. The adapter scopes to one configured
  namespace; callers must not share one sandbox handle across tenants, and RBAC
  must be granted per namespace.

//! Kubernetes API calls. `kube`/`k8s-openapi` types stay in this module tree.

use std::future::Future;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, ListParams, PostParams, PropagationPolicy};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};

use crate::observability::events;

use super::backend::{CommonSpec, InstanceId, PrepareError};
use super::engine::{EngineError, EngineErrorKind, HostReport, OWNER_LABEL_KEY, Recovered};
use super::kubernetes_config::KubernetesConfig;
use super::kubernetes_pod;

const NOT_FOUND: u16 = 404;
const FORBIDDEN: u16 = 403;

/// Live Kubernetes workload engine. Every call uses the namespace from the
/// validated configuration.
pub(crate) struct KubernetesEngine {
    pub(super) pods: Api<Pod>,
    pub(super) image: String,
    pub(super) lifecycle_timeout: Duration,
}

impl KubernetesEngine {
    /// Loads an explicit kubeconfig and builds a namespace-scoped client.
    ///
    /// `KUBECONFIG`, `$HOME/.kube/config`, and in-cluster credentials are never
    /// consulted. No API request is made here.
    pub(crate) async fn connect(
        config: &KubernetesConfig,
        lifecycle_timeout: Duration,
    ) -> Result<Self, Box<EngineError>> {
        let kubeconfig = Kubeconfig::read_from(config.kubeconfig()).map_err(|err| {
            Box::new(engine_error(
                EngineErrorKind::Other,
                format!("kubeconfig cannot be loaded: {err}"),
            ))
        })?;
        let options = KubeConfigOptions {
            context: config.context().map(str::to_string),
            cluster: None,
            user: None,
        };
        let mut kube_config = Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .map_err(|err| {
                Box::new(engine_error(
                    EngineErrorKind::Other,
                    format!("kubeconfig context cannot be resolved: {err}"),
                ))
            })?;
        kube_config.default_namespace = config.namespace().to_string();
        kube_config.connect_timeout = Some(lifecycle_timeout);
        let client = Client::try_from(kube_config).map_err(|err| {
            Box::new(engine_error(
                EngineErrorKind::Other,
                format!("Kubernetes client cannot be built: {err}"),
            ))
        })?;
        events::backend_connect_completed("kubernetes", "ok");
        Ok(Self {
            pods: Api::namespaced(client, config.namespace()),
            image: config.image().to_string(),
            lifecycle_timeout,
        })
    }

    /// Verifies API reachability, namespace access, and that the API server
    /// admits the constructed Pod on a server-side dry run.
    ///
    /// Nothing is persisted: `dryRun=All` performs admission and schema
    /// validation only. The image is not pulled here; the node's kubelet uses
    /// `imagePullPolicy: IfNotPresent`.
    pub(crate) async fn prepare(
        &self,
        spec: &CommonSpec,
        service_account: Option<&str>,
    ) -> Result<(), PrepareError> {
        let result = self.prepare_inner(spec, service_account).await;
        let outcome = match &result {
            Ok(()) => "ok",
            Err(PrepareError::ImageUnavailable { .. }) => "image_unavailable",
            Err(PrepareError::Engine(_)) => "engine_error",
        };
        events::backend_prepare_completed(outcome);
        result
    }

    async fn prepare_inner(
        &self,
        spec: &CommonSpec,
        service_account: Option<&str>,
    ) -> Result<(), PrepareError> {
        self.bounded(self.pods.list(&ListParams::default().limit(1)))
            .await
            .map_err(PrepareError::Engine)?;
        let prepare_nonce = uuid::Uuid::new_v4().to_string();
        let mut pod = kubernetes_pod::build(
            "circus-sandbox-prepare",
            &prepare_nonce,
            spec,
            &self.image,
            service_account,
        );
        pod.metadata.name = None;
        pod.metadata.generate_name = Some("circus-sandbox-prepare-".to_string());
        let params = PostParams {
            dry_run: true,
            ..PostParams::default()
        };
        self.bounded(self.pods.create(&params, &pod))
            .await
            .map_err(PrepareError::Engine)?;
        Ok(())
    }

    /// Creates the Pod and verifies the API server preserved ownership and
    /// security fields. Readiness is not awaited here.
    pub(crate) async fn create_owned(
        &self,
        name: &str,
        spec: &CommonSpec,
        nonce: &str,
        service_account: Option<&str>,
    ) -> Result<InstanceId, EngineError> {
        let span = events::backend_create_span();
        let result = self
            .create_owned_inner(name, spec, nonce, service_account)
            .await;
        let (outcome, create_pending, error_kind) = match &result {
            Ok(_) => ("ok", false, "none"),
            Err(err) => (
                "error",
                err.create_pending,
                super::engine::engine_error_kind(err),
            ),
        };
        {
            let _entered = span.enter();
            events::backend_create_completed(outcome, create_pending, error_kind);
        }
        result
    }

    async fn create_owned_inner(
        &self,
        name: &str,
        spec: &CommonSpec,
        nonce: &str,
        service_account: Option<&str>,
    ) -> Result<InstanceId, EngineError> {
        let pod = kubernetes_pod::build(name, nonce, spec, &self.image, service_account);
        let created = match tokio::time::timeout(
            self.lifecycle_timeout,
            self.pods.create(&PostParams::default(), &pod),
        )
        .await
        {
            Ok(Ok(created)) => created,
            Ok(Err(err)) => return Err(map_create_err(err, name)),
            Err(_) => {
                return Err(EngineError {
                    message: "Kubernetes create exceeded the lifecycle deadline".to_string(),
                    create_pending: true,
                    create_id: Some(name.to_string()),
                    ..engine_error(EngineErrorKind::Other, "")
                });
            }
        };
        if created.metadata.uid.is_none() {
            return Err(EngineError {
                message: "Kubernetes create returned no Pod UID".to_string(),
                create_pending: false,
                create_id: Some(name.to_string()),
                ..engine_error(EngineErrorKind::Other, "")
            });
        }
        if let Err(mismatch) = kubernetes_pod::verify(&created, name, nonce, spec, &self.image) {
            return Err(EngineError {
                create_id: Some(name.to_string()),
                ..mismatch.error()
            });
        }
        Ok(InstanceId::new(name.to_string()))
    }

    /// Waits until the Pod is Ready or its container is running.
    ///
    /// The configured lifecycle deadline bounds the wait. An image pull or
    /// container-configuration failure ends the wait early with a typed error;
    /// lifecycle then destroys the owned Pod.
    pub(crate) async fn start(&self, name: &str) -> Result<(), EngineError> {
        let result = tokio::time::timeout(self.lifecycle_timeout, self.wait_started(name))
            .await
            .unwrap_or_else(|_| {
                Err(engine_error(
                    EngineErrorKind::Other,
                    format!(
                        "Pod {name} did not become ready within {}s",
                        self.lifecycle_timeout.as_secs()
                    ),
                ))
            });
        super::engine::backend_transition("start", &result);
        result
    }

    async fn wait_started(&self, name: &str) -> Result<(), EngineError> {
        loop {
            let pod = self.pods.get(name).await.map_err(map_err)?;
            if let Some(detail) = kubernetes_pod::failure(&pod) {
                return Err(engine_error(EngineErrorKind::Other, detail));
            }
            if kubernetes_pod::started(&pod) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Removes only a Pod carrying this sandbox's owner label, then confirms
    /// absence. A name mismatch, inspection failure, or foreign label is never
    /// deleted.
    pub(crate) async fn recover_owned(&self, name: &str, nonce: &str) -> Recovered {
        let result = self.recover_inner(name, nonce).await;
        events::backend_recovery_completed(recovered_label(&result));
        result
    }

    async fn recover_inner(&self, name: &str, nonce: &str) -> Recovered {
        match self.pods.get(name).await {
            Ok(pod) => {
                if !owned_by(&pod, name, nonce) {
                    return Recovered::Unknown {
                        detail: format!(
                            "Pod {name} exists without this sandbox's owner label; refusing to delete it"
                        ),
                    };
                }
            }
            Err(err) if status_code(&err) == Some(NOT_FOUND) => return Recovered::Absent,
            Err(err) => {
                return Recovered::Unknown {
                    detail: format!("owner confirmation failed; ownership retained: {err}"),
                };
            }
        }
        let params = DeleteParams {
            grace_period_seconds: Some(0),
            propagation_policy: Some(PropagationPolicy::Foreground),
            ..DeleteParams::default()
        };
        if let Err(err) = self.pods.delete(name, &params).await {
            if status_code(&err) == Some(NOT_FOUND) {
                return Recovered::Absent;
            }
            return Recovered::Unknown {
                detail: format!("Pod delete failed; ownership retained: {err}"),
            };
        }
        self.confirm_absent(name).await
    }

    async fn confirm_absent(&self, name: &str) -> Recovered {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.pods.get(name).await {
                Err(err) if status_code(&err) == Some(NOT_FOUND) => return Recovered::Removed,
                Ok(_) if Instant::now() >= deadline => {
                    return Recovered::StillPresent {
                        detail: format!("Pod {name} is still reported after foreground deletion"),
                    };
                }
                Ok(_) => {}
                Err(err) if Instant::now() >= deadline => {
                    return Recovered::Unknown {
                        detail: format!("post-delete inspection failed: {err}"),
                    };
                }
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Reads only what the Pod spec reports.
    pub(crate) async fn host_report(&self, name: &str) -> Result<HostReport, EngineError> {
        let pod = self.pods.get(name).await.map_err(map_err)?;
        Ok(kubernetes_pod::report(&pod))
    }

    async fn bounded<T>(
        &self,
        future: impl Future<Output = Result<T, kube::Error>>,
    ) -> Result<T, EngineError> {
        match tokio::time::timeout(self.lifecycle_timeout, future).await {
            Ok(result) => result.map_err(map_err),
            Err(_) => Err(engine_error(
                EngineErrorKind::Other,
                "Kubernetes API request exceeded the lifecycle deadline",
            )),
        }
    }
}

fn owned_by(pod: &Pod, name: &str, nonce: &str) -> bool {
    let labels = pod.metadata.labels.as_ref();
    pod.metadata.name.as_deref() == Some(name)
        && labels
            .and_then(|labels| labels.get(OWNER_LABEL_KEY))
            .map(String::as_str)
            == Some(nonce)
        && labels
            .and_then(|labels| labels.get("circus.sandbox.name"))
            .map(String::as_str)
            == Some(name)
}

fn recovered_label(recovered: &Recovered) -> &'static str {
    match recovered {
        Recovered::Removed => "removed",
        Recovered::Absent => "absent",
        Recovered::StillPresent { .. } => "still_present",
        Recovered::Unknown { .. } => "unknown",
    }
}

pub(crate) fn engine_error(kind: EngineErrorKind, message: impl Into<String>) -> EngineError {
    EngineError {
        message: message.into(),
        not_found: false,
        kind,
        create_pending: false,
        create_id: None,
        output: None,
        policy: None,
    }
}

fn map_err(err: kube::Error) -> EngineError {
    let code = status_code(&err);
    let kind = match code {
        Some(NOT_FOUND) => EngineErrorKind::NotFound,
        Some(FORBIDDEN) => EngineErrorKind::PermissionDenied,
        _ => EngineErrorKind::Other,
    };
    EngineError {
        not_found: code == Some(NOT_FOUND),
        ..engine_error(kind, err.to_string())
    }
}

fn map_create_err(err: kube::Error, name: &str) -> EngineError {
    let code = status_code(&err);
    let pending = code.is_none_or(|code| code >= 500);
    let mut mapped = map_err(err);
    mapped.create_pending = pending;
    if pending {
        // The request may have been accepted even though no response arrived;
        // retaining the name lets lifecycle verify labels before deletion.
        mapped.create_id = Some(name.to_string());
    }
    mapped
}

fn status_code(err: &kube::Error) -> Option<u16> {
    match err {
        kube::Error::Api(status) if status.code != 0 => Some(status.code),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    #[test]
    fn ownership_requires_both_labels() {
        let owner = uuid::Uuid::new_v4().to_string();
        let mut pod = Pod {
            metadata: ObjectMeta {
                name: Some("name".to_string()),
                ..ObjectMeta::default()
            },
            ..Pod::default()
        };
        assert!(!owned_by(&pod, "name", &owner));
        pod.metadata.labels = Some(
            [
                (OWNER_LABEL_KEY.to_string(), owner.clone()),
                ("circus.sandbox.name".to_string(), "name".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        assert!(owned_by(&pod, "name", &owner));
        let foreign = uuid::Uuid::new_v4().to_string();
        assert!(!owned_by(&pod, "other", &owner));
        assert!(!owned_by(&pod, "name", &foreign));
    }
}

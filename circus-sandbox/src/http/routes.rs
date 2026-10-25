//! Route table, authentication middleware, and handlers.
//!
//! The table below is the single registration list: [`router`] iterates it and
//! the OpenAPI drift test reads it, so a route cannot exist in only one place.

use axum::body::{Body, Bytes};
use axum::extract::rejection::PathRejection;
use axum::extract::{Extension, FromRequestParts, Path, State};
use axum::http::header::{self, HeaderName};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, post, put};
use axum::{Json, Router};
use serde::de::DeserializeOwned;

use crate::api::SandboxPhase;

use super::dto::{
    CaptureDto, CreateSandboxRequest, ExecBody, ExecDto, FilePath, HealthBody, IdPath, SandboxBody,
    SnapshotBody, SnapshotPath,
};
use super::error::{ApiError, from_sandbox};
use super::state::{AppState, Tenant};

const TENANT_HEADER: HeaderName = HeaderName::from_static("x-circus-tenant");

/// One registered route. `method` and `openapi_path` are the documented form.
#[derive(Debug, Clone)]
pub(super) struct RouteSpec {
    pub(super) id: RouteId,
    pub(super) axum_path: &'static str,
    // Read by the OpenAPI drift test, which compares this table with
    // `docs/openapi.json` and the live router.
    #[allow(dead_code)]
    pub(super) openapi_path: &'static str,
    #[allow(dead_code)]
    pub(super) method: Method,
}

/// Stable route identity, used for exhaustive handler registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteId {
    Health,
    Create,
    Status,
    Start,
    Pause,
    Resume,
    Delete,
    Exec,
    ReadFile,
    WriteFile,
    CaptureSnapshot,
    RestoreSnapshot,
}

const fn route(
    id: RouteId,
    axum_path: &'static str,
    openapi_path: &'static str,
    method: Method,
) -> RouteSpec {
    RouteSpec {
        id,
        axum_path,
        openapi_path,
        method,
    }
}

pub(super) const ROUTES: &[RouteSpec] = &[
    route(RouteId::Health, "/healthz", "/healthz", Method::GET),
    route(
        RouteId::Create,
        "/v1/sandboxes",
        "/v1/sandboxes",
        Method::POST,
    ),
    route(
        RouteId::Status,
        "/v1/sandboxes/{id}",
        "/v1/sandboxes/{id}",
        Method::GET,
    ),
    route(
        RouteId::Start,
        "/v1/sandboxes/{id}/start",
        "/v1/sandboxes/{id}/start",
        Method::POST,
    ),
    route(
        RouteId::Pause,
        "/v1/sandboxes/{id}/pause",
        "/v1/sandboxes/{id}/pause",
        Method::POST,
    ),
    route(
        RouteId::Resume,
        "/v1/sandboxes/{id}/resume",
        "/v1/sandboxes/{id}/resume",
        Method::POST,
    ),
    route(
        RouteId::Delete,
        "/v1/sandboxes/{id}",
        "/v1/sandboxes/{id}",
        Method::DELETE,
    ),
    route(
        RouteId::Exec,
        "/v1/sandboxes/{id}/exec",
        "/v1/sandboxes/{id}/exec",
        Method::POST,
    ),
    route(
        RouteId::ReadFile,
        "/v1/sandboxes/{id}/files/{*path}",
        "/v1/sandboxes/{id}/files/{path}",
        Method::GET,
    ),
    route(
        RouteId::WriteFile,
        "/v1/sandboxes/{id}/files/{*path}",
        "/v1/sandboxes/{id}/files/{path}",
        Method::PUT,
    ),
    route(
        RouteId::CaptureSnapshot,
        "/v1/sandboxes/{id}/snapshots",
        "/v1/sandboxes/{id}/snapshots",
        Method::POST,
    ),
    route(
        RouteId::RestoreSnapshot,
        "/v1/sandboxes/{id}/snapshots/{snapshot_id}/restore",
        "/v1/sandboxes/{id}/snapshots/{snapshot_id}/restore",
        Method::POST,
    ),
];

/// Builds the complete router. Authentication wraps every route, the
/// not-found fallback, and the method-not-allowed fallback.
pub(super) fn router(state: AppState) -> Router {
    let mut router = Router::new();
    for spec in ROUTES {
        router = router.route(spec.axum_path, method_router(spec.id));
    }
    router
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

fn method_router(id: RouteId) -> MethodRouter<AppState> {
    match id {
        RouteId::Health => get(healthz),
        RouteId::Create => post(create_sandbox),
        RouteId::Status => get(get_sandbox),
        RouteId::Start => post(start_sandbox),
        RouteId::Pause => post(pause_sandbox),
        RouteId::Resume => post(resume_sandbox),
        RouteId::Delete => delete(delete_sandbox),
        RouteId::Exec => post(exec),
        RouteId::ReadFile => get(read_file),
        RouteId::WriteFile => put(write_file),
        RouteId::CaptureSnapshot => post(capture_snapshot),
        RouteId::RestoreSnapshot => post(restore_snapshot),
    }
}

async fn authenticate(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: Next,
) -> Result<Response, ApiError> {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer);
    let Some(presented) = presented else {
        return Err(ApiError::unauthorized());
    };
    if !state.token().matches(presented) {
        return Err(ApiError::unauthorized());
    }
    let tenant = request
        .headers()
        .get(&TENANT_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let Some(tenant) = tenant else {
        return Err(ApiError::forbidden());
    };
    if !state.tenant_allowed(&tenant) {
        return Err(ApiError::forbidden());
    }
    request.extensions_mut().insert(Tenant(tenant));
    Ok(next.run(request).await)
}

fn bearer(value: &str) -> Option<&[u8]> {
    let (scheme, credential) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let credential = credential.trim();
    if credential.is_empty() || credential.contains(' ') {
        return None;
    }
    Some(credential.as_bytes())
}

async fn not_found() -> ApiError {
    ApiError::not_found()
}

async fn method_not_allowed() -> ApiError {
    ApiError::method_not_allowed()
}

async fn healthz() -> Json<HealthBody> {
    Json(HealthBody { status: "ok" })
}

async fn create_sandbox(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    body: Body,
) -> Result<(StatusCode, Json<SandboxBody>), ApiError> {
    let request: CreateSandboxRequest = read_json(&state, body).await?;
    let backend = request.backend.into_config()?;
    let config = request.sandbox.into_config()?;
    let (id, _) = state
        .create_sandbox(&tenant.0, backend, config)
        .await
        .map_err(from_sandbox)?;
    Ok((
        StatusCode::CREATED,
        Json(SandboxBody::new(id, SandboxPhase::Created, None)),
    ))
}

async fn get_sandbox(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<IdPath>,
) -> Result<Json<SandboxBody>, ApiError> {
    let path = path.0;
    let sandbox = lookup(&state, &tenant, &path.id)?;
    let status = sandbox.status().await.map_err(from_sandbox)?;
    Ok(Json(SandboxBody::new(
        path.id,
        status.phase,
        status.enforcement,
    )))
}

async fn start_sandbox(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<IdPath>,
) -> Result<StatusCode, ApiError> {
    lookup(&state, &tenant, &path.0.id)?
        .start()
        .await
        .map_err(from_sandbox)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn pause_sandbox(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<IdPath>,
) -> Result<StatusCode, ApiError> {
    lookup(&state, &tenant, &path.0.id)?
        .pause()
        .await
        .map_err(from_sandbox)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_sandbox(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<IdPath>,
) -> Result<StatusCode, ApiError> {
    lookup(&state, &tenant, &path.0.id)?
        .resume()
        .await
        .map_err(from_sandbox)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_sandbox(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<IdPath>,
) -> Result<StatusCode, ApiError> {
    let id = path.0.id;
    let sandbox = lookup(&state, &tenant, &id)?;
    sandbox.destroy().await.map_err(from_sandbox)?;
    state.remove_sandbox(&tenant.0, &id, &sandbox);
    Ok(StatusCode::NO_CONTENT)
}

async fn exec(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<IdPath>,
    body: Body,
) -> Result<Json<ExecBody>, ApiError> {
    let sandbox = lookup(&state, &tenant, &path.0.id)?;
    let request: ExecDto = read_json(&state, body).await?;
    let output = sandbox
        .exec(request.into_request()?)
        .await
        .map_err(from_sandbox)?;
    Ok(Json(ExecBody::from(output)))
}

async fn read_file(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<FilePath>,
) -> Result<Response, ApiError> {
    let path = path.0;
    let sandbox = lookup(&state, &tenant, &path.id)?;
    let bytes = sandbox.read_file(&path.path).await.map_err(from_sandbox)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn write_file(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<FilePath>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    let path = path.0;
    let sandbox = lookup(&state, &tenant, &path.id)?;
    let bytes = read_body(&state, body).await?;
    sandbox
        .write_file(&path.path, &bytes)
        .await
        .map_err(from_sandbox)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn capture_snapshot(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<IdPath>,
    body: Body,
) -> Result<(StatusCode, Json<SnapshotBody>), ApiError> {
    let sandbox = lookup(&state, &tenant, &path.0.id)?;
    let request: CaptureDto = read_json(&state, body).await?;
    let (budget, max_archive_bytes) = request.budget()?;
    if budget.max_total_bytes() > state.snapshot_max_bytes()
        || budget.max_entries() > state.snapshot_max_entries()
        || max_archive_bytes == 0
        || max_archive_bytes > state.snapshot_max_bytes() as u64
    {
        return Err(ApiError::limit_exceeded());
    }
    let snapshot = sandbox
        .capture_workspace_snapshot(budget, max_archive_bytes)
        .await
        .map_err(from_sandbox)?;
    let entries = snapshot.entry_count();
    let total_bytes = snapshot.total_bytes();
    let snapshot_id = state
        .retain_snapshot(&tenant.0, snapshot)
        .ok_or_else(ApiError::limit_exceeded)?;
    Ok((
        StatusCode::CREATED,
        Json(SnapshotBody {
            snapshot_id,
            entries,
            total_bytes,
        }),
    ))
}

async fn restore_snapshot(
    State(state): State<AppState>,
    Extension(tenant): Extension<Tenant>,
    path: ApiPath<SnapshotPath>,
) -> Result<StatusCode, ApiError> {
    let path = path.0;
    let sandbox = lookup(&state, &tenant, &path.id)?;
    let snapshot = state
        .snapshot(&tenant.0, &path.snapshot_id)
        .ok_or_else(ApiError::not_found)?;
    sandbox
        .restore_workspace_snapshot(snapshot)
        .await
        .map_err(from_sandbox)?;
    Ok(StatusCode::NO_CONTENT)
}

fn lookup(
    state: &AppState,
    tenant: &Tenant,
    id: &str,
) -> Result<std::sync::Arc<crate::api::Sandbox>, ApiError> {
    state.sandbox(&tenant.0, id).ok_or_else(ApiError::not_found)
}

async fn read_json<T: DeserializeOwned>(state: &AppState, body: Body) -> Result<T, ApiError> {
    let bytes = read_body(state, body).await?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::invalid_json())
}

async fn read_body(state: &AppState, body: Body) -> Result<Bytes, ApiError> {
    axum::body::to_bytes(body, state.max_body_bytes())
        .await
        .map_err(|_| ApiError::limit_exceeded())
}

/// Path extractor whose rejection is a stable JSON error body.
pub(super) struct ApiPath<T>(pub(super) T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(value) = Path::<T>::from_request_parts(parts, state)
            .await
            .map_err(|_: PathRejection| ApiError::invalid_request())?;
        Ok(Self(value))
    }
}

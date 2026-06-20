use axum::{
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use noema_core::engine::Engine;
use std::sync::Arc;

#[derive(Clone)]
struct UiState {
    engine: Arc<Engine>,
}

pub async fn run(engine: Arc<Engine>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let state = UiState { engine };
    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.png", get(favicon))
        .route("/logo.png", get(favicon))
        .route("/api/manifests", get(api_manifests))
        .route("/api/manifest/:id", get(api_manifest))
        .route("/api/import", post(api_import))
        .route("/api/download/:id", post(api_download))
        .route("/api/install/:id", post(api_install))
        .route("/api/cache", get(api_cache))
        .route("/api/health", get(api_health))
        .route("/api/diagnostics", get(api_diagnostics))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    println!("noema ui: open http://{bound} in your browser");
    axum::serve(listener, app).await?;
    Ok(())
}

fn e500(e: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": e.to_string()})),
    )
        .into_response()
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// The app logo, served for the dashboard favicon.
const LOGO_PNG: &[u8] = include_bytes!("../../../assets/logo.png");

async fn favicon() -> Response {
    ([(axum::http::header::CONTENT_TYPE, "image/png")], LOGO_PNG).into_response()
}

async fn api_manifests(State(s): State<UiState>) -> Response {
    match s.engine.list_manifests() {
        Ok(list) => Json(
            list.into_iter()
                .map(|m| {
                    serde_json::json!({
                        "manifest_id": m.manifest_id,
                        "model_name": m.model_name,
                        "revision": m.revision,
                        "license_spdx": m.license_spdx,
                        "redistribution": m.redistribution.as_str(),
                        "gated": m.gated,
                        "signed": m.signed,
                        "imported_at": m.imported_at,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => e500(e),
    }
}

async fn api_manifest(State(s): State<UiState>, AxPath(id): AxPath<String>) -> Response {
    let manifest = match s.engine.get_manifest(&id) {
        Ok(Some(m)) => m,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such manifest").into_response(),
        Err(e) => return e500(e),
    };
    let report = s.engine.verify_manifest(&id).ok();
    let plan = s.engine.plan_download(&id).unwrap_or_default();
    let plan_json: Vec<_> = plan
        .into_iter()
        .map(|(path, p)| {
            serde_json::json!({
                "artifact": path,
                "eligible": p.eligible.iter().map(|x| serde_json::json!({
                    "source_id": x.source_id, "score": x.score
                })).collect::<Vec<_>>(),
                "excluded": p.excluded.iter().map(|x| serde_json::json!({
                    "source_id": x.source_id, "reason": x.reason
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    // Which artifacts are already cached?
    let cached: std::collections::HashSet<String> = s
        .engine
        .list_cache()
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.blake3)
        .collect();
    let artifacts: Vec<_> = manifest
        .artifacts
        .iter()
        .map(|a| {
            serde_json::json!({
                "path": a.path,
                "role": a.role,
                "size_bytes": a.size_bytes,
                "format": a.format,
                "blake3": a.hashes.blake3,
                "cached": cached.contains(&a.hashes.blake3),
                "sources": a.sources.iter().map(|src| serde_json::json!({
                    "class": format!("{:?}", src.class()),
                    "source_id": src.source_id(),
                    "auth": format!("{:?}", src.auth()),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "manifest_id": manifest.manifest_id,
        "model": manifest.model,
        "license": manifest.license,
        "access": manifest.access,
        "provenance": manifest.provenance,
        "publisher": manifest.publisher.id,
        "signed": report.as_ref().map(|r| r.is_signed()).unwrap_or(false),
        "valid_signatures": report.as_ref().map(|r| r.valid_signatures.clone()).unwrap_or_default(),
        "artifacts": artifacts,
        "plan": plan_json,
    }))
    .into_response()
}

async fn api_import(State(s): State<UiState>, body: axum::body::Bytes) -> Response {
    match s.engine.import_manifest(&body) {
        Ok(r) => Json(serde_json::json!({
            "manifest_id": r.manifest_id,
            "signed": r.report.is_signed(),
            "policy_allowed": r.policy.allowed,
            "policy_reason": r.policy.reason,
            "warnings": r.policy.warnings,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn api_download(State(s): State<UiState>, AxPath(id): AxPath<String>) -> Response {
    match s.engine.download(&id, None).await {
        Ok(out) => Json(serde_json::json!({
            "manifest_id": out.manifest_id,
            "artifacts": out.artifacts.iter().map(|a| serde_json::json!({
                "artifact_path": a.artifact_path,
                "from_cache": a.from_cache,
                "source_id": a.source_id,
                "size_bytes": a.size_bytes,
                "warnings": a.warnings,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn api_install(
    State(s): State<UiState>,
    AxPath(id): AxPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let target = String::from_utf8_lossy(&body).trim().to_string();
    if target.is_empty() {
        return (StatusCode::BAD_REQUEST, "target path required").into_response();
    }
    match s
        .engine
        .materialize_install(&id, std::path::Path::new(&target))
    {
        Ok(views) => Json(serde_json::json!({
            "installed": views.iter().map(|v| serde_json::json!({
                "artifact_path": v.artifact_path,
                "dest": v.dest.to_string_lossy(),
                "link_kind": v.link_kind.as_str(),
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn api_cache(State(s): State<UiState>) -> Response {
    match s.engine.list_cache() {
        Ok(list) => Json(
            list.into_iter()
                .map(|b| serde_json::json!({
                    "blake3": b.blake3, "size_bytes": b.size_bytes, "state": b.state, "committed_at": b.committed_at
                }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => e500(e),
    }
}

async fn api_health(State(s): State<UiState>) -> Response {
    match s.engine.report_source_health() {
        Ok(list) => Json(
            list.into_iter()
                .map(|h| serde_json::json!({
                    "source_id": h.source_id, "success": h.success_count, "failure": h.failure_count,
                    "integrity_failures": h.integrity_failures, "banned": h.banned, "last_latency_ms": h.last_latency_ms
                }))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => e500(e),
    }
}

async fn api_diagnostics(State(s): State<UiState>) -> Response {
    match s.engine.export_diagnostics() {
        Ok(v) => Json(v).into_response(),
        Err(e) => e500(e),
    }
}

const INDEX_HTML: &str = include_str!("ui.html");

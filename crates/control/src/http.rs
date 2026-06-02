//! HTTP routes.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;

use crate::{audit, ids};

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    token: String,
}

pub async fn serve(port: u16, token: String, pool: SqlitePool) -> Result<()> {
    let state = AppState { pool, token };
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/peers", post(bind_peer))
        .route("/v1/peers/:user_id", get(get_peer))
        .route("/v1/audit", get(list_audit))
        .route("/v1/_internal/audit", post(ingest_audit))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let bind: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    tracing::info!("xyzen-control listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---- handlers ------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
struct BindPeerReq {
    user_id: String,
}

#[derive(Debug, Serialize)]
struct PeerResp {
    user_id: String,
    peer_id: String,
}

async fn bind_peer(
    headers: HeaderMap,
    State(s): State<AppState>,
    Json(req): Json<BindPeerReq>,
) -> Result<Json<PeerResp>, AppError> {
    require_token(&headers, &s.token)?;
    if req.user_id.trim().is_empty() {
        return Err(AppError::bad("user_id required"));
    }

    // Idempotent: if already bound, return existing.
    if let Some((peer_id,)) =
        sqlx::query_as::<_, (String,)>("SELECT peer_id FROM peers WHERE user_id = ?")
            .bind(&req.user_id)
            .fetch_optional(&s.pool)
            .await
            .map_err(AppError::db)?
    {
        return Ok(Json(PeerResp {
            user_id: req.user_id,
            peer_id,
        }));
    }

    let peer_id = ids::fresh(&s.pool).await.map_err(AppError::internal)?;
    sqlx::query("INSERT INTO peers (user_id, peer_id, created_at) VALUES (?, ?, ?)")
        .bind(&req.user_id)
        .bind(&peer_id)
        .bind(chrono::Utc::now().timestamp())
        .execute(&s.pool)
        .await
        .map_err(AppError::db)?;
    audit::record(
        &s.pool,
        "peer_bound",
        Some(&peer_id),
        None,
        None,
        Some(serde_json::json!({ "user_id": req.user_id })),
    )
    .await;

    Ok(Json(PeerResp {
        user_id: req.user_id,
        peer_id,
    }))
}

async fn get_peer(
    headers: HeaderMap,
    axum::extract::Path(user_id): axum::extract::Path<String>,
    State(s): State<AppState>,
) -> Result<Json<PeerResp>, AppError> {
    require_token(&headers, &s.token)?;
    let row: Option<(String,)> =
        sqlx::query_as("SELECT peer_id FROM peers WHERE user_id = ?")
            .bind(&user_id)
            .fetch_optional(&s.pool)
            .await
            .map_err(AppError::db)?;
    let (peer_id,) = row.ok_or_else(|| AppError::not_found("user_id not bound"))?;
    Ok(Json(PeerResp { user_id, peer_id }))
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    peer_id: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct IngestAudit {
    kind: String,
    peer_id: Option<String>,
    controller_peer_id: Option<String>,
    addr: Option<String>,
    meta: Option<serde_json::Value>,
}

async fn ingest_audit(
    headers: HeaderMap,
    State(s): State<AppState>,
    Json(ev): Json<IngestAudit>,
) -> Result<StatusCode, AppError> {
    require_token(&headers, &s.token)?;
    audit::record(
        &s.pool,
        &ev.kind,
        ev.peer_id.as_deref(),
        ev.controller_peer_id.as_deref(),
        ev.addr.as_deref(),
        ev.meta,
    )
    .await;
    Ok(StatusCode::ACCEPTED)
}

async fn list_audit(
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
    State(s): State<AppState>,
) -> Result<Json<Vec<audit::AuditRow>>, AppError> {
    require_token(&headers, &s.token)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let mut peer_filter = q.peer_id.clone();
    // If user_id is provided, resolve to a peer_id once.
    if peer_filter.is_none() {
        if let Some(uid) = &q.user_id {
            if let Some((p,)) =
                sqlx::query_as::<_, (String,)>("SELECT peer_id FROM peers WHERE user_id = ?")
                    .bind(uid)
                    .fetch_optional(&s.pool)
                    .await
                    .map_err(AppError::db)?
            {
                peer_filter = Some(p);
            } else {
                return Ok(Json(vec![]));
            }
        }
    }
    let since = q.since.unwrap_or(0);
    let rows: Vec<audit::AuditRow> = match peer_filter {
        Some(pid) => sqlx::query_as(
            "SELECT id, ts, kind, peer_id, controller_peer_id, addr, meta_json \
             FROM audit \
             WHERE ts >= ? AND (peer_id = ? OR controller_peer_id = ?) \
             ORDER BY id DESC LIMIT ?",
        )
        .bind(since)
        .bind(&pid)
        .bind(&pid)
        .bind(limit)
        .fetch_all(&s.pool)
        .await
        .map_err(AppError::db)?,
        None => sqlx::query_as(
            "SELECT id, ts, kind, peer_id, controller_peer_id, addr, meta_json \
             FROM audit WHERE ts >= ? ORDER BY id DESC LIMIT ?",
        )
        .bind(since)
        .bind(limit)
        .fetch_all(&s.pool)
        .await
        .map_err(AppError::db)?,
    };
    Ok(Json(rows))
}

// ---- auth + errors -------------------------------------------------------

fn require_token(headers: &HeaderMap, expected: &str) -> Result<(), AppError> {
    let v = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::unauthorized("missing Authorization"))?;
    let bearer = v
        .strip_prefix("Bearer ")
        .ok_or_else(|| AppError::unauthorized("expected Bearer token"))?;
    if !ct_eq(bearer.as_bytes(), expected.as_bytes()) {
        return Err(AppError::unauthorized("invalid token"));
    }
    Ok(())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    msg: String,
}

impl AppError {
    fn bad(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            msg: m.into(),
        }
    }
    fn unauthorized(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            msg: m.into(),
        }
    }
    fn not_found(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            msg: m.into(),
        }
    }
    fn db(e: sqlx::Error) -> Self {
        tracing::error!("sqlx: {e}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: "database error".into(),
        }
    }
    fn internal(e: anyhow::Error) -> Self {
        tracing::error!("internal: {e}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: "internal error".into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.msg }))).into_response()
    }
}

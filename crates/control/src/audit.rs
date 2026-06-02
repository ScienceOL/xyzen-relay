//! Audit log helpers.

use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AuditRow {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub peer_id: Option<String>,
    pub controller_peer_id: Option<String>,
    pub addr: Option<String>,
    pub meta_json: Option<String>,
}

/// Append a structured event. Best-effort — never fail the caller.
pub async fn record(
    pool: &SqlitePool,
    kind: &str,
    peer_id: Option<&str>,
    controller_peer_id: Option<&str>,
    addr: Option<&str>,
    meta: Option<serde_json::Value>,
) {
    let ts = chrono::Utc::now().timestamp();
    let meta_str = meta.map(|v| v.to_string());
    let res = sqlx::query(
        "INSERT INTO audit (ts, kind, peer_id, controller_peer_id, addr, meta_json) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(ts)
    .bind(kind)
    .bind(peer_id)
    .bind(controller_peer_id)
    .bind(addr)
    .bind(meta_str)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!("audit insert failed: {e}");
    }
}

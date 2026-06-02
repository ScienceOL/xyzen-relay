//! Generate 9-digit numeric peer IDs that look like RustDesk's.
//!
//! Uses `OsRng` directly — it's `Send`, unlike `thread_rng()` which holds
//! an `Rc` and breaks the `Send` bound axum needs across `.await`.

use rand::{rngs::OsRng, RngCore};
use sqlx::SqlitePool;

const SPAN: u64 = 900_000_000; // 999_999_999 - 100_000_000 + 1
const BASE: u64 = 100_000_000;

fn random_id() -> String {
    let mut buf = [0u8; 8];
    OsRng.fill_bytes(&mut buf);
    let n = u64::from_le_bytes(buf) % SPAN + BASE;
    n.to_string()
}

/// Pick a fresh 9-digit ID that doesn't collide with `peers.peer_id`.
/// Loops up to 16 times — collision odds are tiny.
pub async fn fresh(pool: &SqlitePool) -> anyhow::Result<String> {
    for _ in 0..16 {
        let s = random_id();
        let exists: Option<(String,)> =
            sqlx::query_as("SELECT peer_id FROM peers WHERE peer_id = ?")
                .bind(&s)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            return Ok(s);
        }
    }
    anyhow::bail!("failed to allocate a unique peer id after 16 tries")
}

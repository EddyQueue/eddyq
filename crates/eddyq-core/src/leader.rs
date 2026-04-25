use sqlx::PgPool;
use uuid::Uuid;
use crate::error::Result;

pub const MAINTENANCE_ROLE: &str = "maintenance";

/// LISTEN/NOTIFY channel a resigning leader uses to wake other nodes for an
/// immediate election, so graceful shutdowns don't leave a `lease_secs / 3`
/// gap before maintenance resumes.
pub(crate) const LEADER_RESIGN_CHANNEL: &str = "eddyq_leader_resign";

/// Try to win or refresh leadership. Returns true if this worker_id is now leader.
/// Single upsert handles: first boot (insert), lease expired (take over), already leader (refresh), other leader active (no-op).
pub async fn try_elect(pool: &PgPool, worker_id: Uuid, role: &str, lease_secs: u64) -> Result<bool> {
    // The CASE logic: expired → take over; same worker → refresh; other valid leader → keep theirs
    let row: Option<(Uuid,)> = sqlx::query_as(r#"
        INSERT INTO eddyq_leader (role, worker_id, expires_at, elected_at)
        VALUES ($1, $2, NOW() + make_interval(secs => $3::double precision), NOW())
        ON CONFLICT (role) DO UPDATE
          SET worker_id  = CASE
                             WHEN eddyq_leader.expires_at < NOW() THEN excluded.worker_id
                             ELSE eddyq_leader.worker_id
                           END,
              expires_at = CASE
                             WHEN eddyq_leader.expires_at < NOW()         THEN excluded.expires_at
                             WHEN eddyq_leader.worker_id = excluded.worker_id THEN excluded.expires_at
                             ELSE eddyq_leader.expires_at
                           END,
              elected_at = CASE
                             WHEN eddyq_leader.expires_at < NOW() THEN excluded.elected_at
                             ELSE eddyq_leader.elected_at
                           END
        RETURNING worker_id
    "#)
    .bind(role)
    .bind(worker_id)
    .bind(lease_secs as f64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id == worker_id).unwrap_or(false))
}

/// Expire our lease so another pod can immediately take over, and NOTIFY peers
/// on `LEADER_RESIGN_CHANNEL` so they fire an immediate election instead of
/// waiting for the next refresh tick.
pub async fn resign(pool: &PgPool, worker_id: Uuid, role: &str) -> Result<()> {
    sqlx::query("UPDATE eddyq_leader SET expires_at = NOW() WHERE role = $1 AND worker_id = $2")
        .bind(role)
        .bind(worker_id)
        .execute(pool)
        .await?;
    let _ = sqlx::query("SELECT pg_notify($1, $2)")
        .bind(LEADER_RESIGN_CHANNEL)
        .bind(role)
        .execute(pool)
        .await;
    Ok(())
}

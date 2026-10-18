use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
pub struct Reservation {
    pub id: Uuid,
    pub user_id: Uuid,
    pub spot_id: String,
    pub status: String,
    pub created_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub enum CreateError {
    Unavailable,
    Database(sqlx::Error),
}

#[derive(Clone, Copy, Debug)]
pub enum FinishReason {
    Cancel { user_id: Uuid },
    Expired,
    UnconfirmedOccupancy,
}

#[derive(Debug)]
pub struct Transition {
    pub spot_id: String,
    pub user_id: Option<Uuid>,
    pub spot_status: Option<&'static str>,
}

pub async fn create(
    pool: &PgPool,
    spot_id: &str,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
) -> Result<Reservation, CreateError> {
    let mut tx = pool.begin().await.map_err(CreateError::Database)?;
    match create_in_transaction(&mut tx, spot_id, user_id, expires_at).await {
        Ok(reservation) => {
            tx.commit().await.map_err(CreateError::Database)?;
            Ok(reservation)
        }
        Err(error) => {
            tx.rollback().await.map_err(CreateError::Database)?;
            Err(error)
        }
    }
}

// The caller owns commit/rollback (or a savepoint), so authorization and the
// reservation claim can retain the same transaction and spot-first lock order.
pub async fn create_in_transaction(
    connection: &mut PgConnection,
    spot_id: &str,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
) -> Result<Reservation, CreateError> {
    let claimed = sqlx::query(
        "UPDATE spots SET status = 'reserved', last_updated = NOW() \
         WHERE id = $1 AND status = 'free' AND is_active",
    )
    .bind(spot_id)
    .execute(&mut *connection)
    .await
    .map_err(CreateError::Database)?;
    if claimed.rows_affected() == 0 {
        return Err(CreateError::Unavailable);
    }
    sqlx::query_as::<_, Reservation>(
        "INSERT INTO reservations (id, user_id, spot_id, status, expires_at) \
         VALUES ($1, $2, $3, 'active', $4) \
         RETURNING id, user_id, spot_id, status, created_at, expires_at, completed_at",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(spot_id)
    .bind(expires_at)
    .fetch_one(&mut *connection)
    .await
    .map_err(|error| {
        let active_conflict = error.as_database_error().is_some_and(|database_error| {
            database_error.code().as_deref() == Some("23505")
                && database_error.constraint() == Some("reservations_one_active_per_spot")
        });
        if active_conflict {
            CreateError::Unavailable
        } else {
            CreateError::Database(error)
        }
    })
}

pub async fn finish(
    pool: &PgPool,
    reservation_id: Uuid,
    reason: FinishReason,
) -> Result<Option<Transition>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Locate the spot without locking the reservation: every transition locks the spot first.
    let spot_id = sqlx::query_scalar::<_, String>(
        "SELECT spot_id FROM reservations WHERE id = $1 AND status = 'active'",
    )
    .bind(reservation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(spot_id) = spot_id else {
        tx.rollback().await?;
        return Ok(None);
    };

    let spot_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM spots WHERE id = $1 FOR NO KEY UPDATE")
            .bind(&spot_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(spot_status) = spot_status else {
        tx.rollback().await?;
        return Ok(None);
    };

    if matches!(reason, FinishReason::UnconfirmedOccupancy) && spot_status != "occupied" {
        tx.rollback().await?;
        return Ok(None);
    }

    let (terminal_status, owner, must_be_expired) = match reason {
        FinishReason::Cancel { user_id } => ("cancelled", Some(user_id), false),
        FinishReason::Expired => ("expired", None, true),
        FinishReason::UnconfirmedOccupancy => ("cancelled", None, false),
    };

    // Recheck ownership, the active state, and expiration after acquiring the spot lock.
    let finished = sqlx::query_as::<_, (String, Option<Uuid>)>(
        "UPDATE reservations SET status = $3 \
         WHERE id = $1 AND spot_id = $2 AND status = 'active' \
         AND ($4::uuid IS NULL OR user_id = $4) \
         AND (NOT $5::boolean OR expires_at < clock_timestamp()) \
         RETURNING spot_id, user_id",
    )
    .bind(reservation_id)
    .bind(&spot_id)
    .bind(terminal_status)
    .bind(owner)
    .bind(must_be_expired)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((spot_id, user_id)) = finished else {
        tx.rollback().await?;
        return Ok(None);
    };

    let mut changed_status = None;
    if !matches!(reason, FinishReason::UnconfirmedOccupancy) {
        let released = sqlx::query(
            "UPDATE spots SET status = 'free', last_updated = NOW() \
             WHERE id = $1 AND status = 'reserved'",
        )
        .bind(&spot_id)
        .execute(&mut *tx)
        .await?;
        if released.rows_affected() > 0 {
            changed_status = Some("free");
        }
    }

    tx.commit().await?;
    Ok(Some(Transition {
        spot_id,
        user_id,
        spot_status: changed_status,
    }))
}

#[derive(Debug)]
pub enum ConfirmError {
    NotFound,
    NotReady,
    Database(sqlx::Error),
}

pub async fn confirm(
    pool: &PgPool,
    reservation_id: Uuid,
    user_id: Uuid,
) -> Result<(), ConfirmError> {
    let mut tx = pool.begin().await.map_err(ConfirmError::Database)?;
    let spot_id: Option<String> = sqlx::query_scalar(
        "SELECT spot_id FROM reservations WHERE id=$1 AND user_id=$2 AND status='active'",
    )
    .bind(reservation_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ConfirmError::Database)?;
    let spot_id = spot_id.ok_or(ConfirmError::NotFound)?;
    // Match creation, observation and finish: acquire the spot lock first.
    let spot: Option<(String, bool)> =
        sqlx::query_as("SELECT status,is_active FROM spots WHERE id=$1 FOR NO KEY UPDATE")
            .bind(&spot_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(ConfirmError::Database)?;
    if !matches!(spot, Some((ref status, true)) if status == "occupied") {
        return Err(ConfirmError::NotReady);
    }
    let updated = sqlx::query("UPDATE reservations SET status='completed',completed_at=NOW() WHERE id=$1 AND user_id=$2 AND status='active' AND expires_at>clock_timestamp()")
        .bind(reservation_id).bind(user_id).execute(&mut *tx).await.map_err(ConfirmError::Database)?;
    if updated.rows_affected() == 0 {
        return Err(ConfirmError::NotReady);
    }
    sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,$2)")
        .bind(user_id)
        .bind(&spot_id)
        .execute(&mut *tx)
        .await
        .map_err(ConfirmError::Database)?;
    sqlx::query("DELETE FROM user_occupancy_history WHERE id IN (SELECT id FROM user_occupancy_history WHERE user_id=$1 ORDER BY occupied_at DESC OFFSET 30)")
        .bind(user_id).execute(&mut *tx).await.map_err(ConfirmError::Database)?;
    tx.commit().await.map_err(ConfirmError::Database)
}

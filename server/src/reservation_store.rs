use crate::reservation_policy::ReservationPolicy;
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
    UserLimit,
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
    policy: &ReservationPolicy,
) -> Result<Reservation, CreateError> {
    let mut tx = pool.begin().await.map_err(CreateError::Database)?;
    match create_in_transaction(&mut tx, spot_id, user_id, policy).await {
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
    policy: &ReservationPolicy,
) -> Result<Reservation, CreateError> {
    policy
        .validate()
        .map_err(|error| CreateError::Database(sqlx::Error::Protocol(error.to_string())))?;
    // Quota coordination must not lock the account row: security updates and
    // session revocation must remain able to cancel a blocked reservation.
    // Hash collisions only serialize unrelated accounts; they cannot bypass quota.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text,19088743))")
        .bind(user_id.to_string())
        .execute(&mut *connection)
        .await
        .map_err(CreateError::Database)?;
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM reservations WHERE user_id=$1 AND status='active'",
    )
    .bind(user_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(CreateError::Database)?;
    if active >= policy.max_active_per_user {
        return Err(CreateError::UserLimit);
    }
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
         VALUES ($1, $2, $3, 'active', NOW() + make_interval(secs => $4)) \
         RETURNING id, user_id, spot_id, status, created_at, expires_at, completed_at",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(spot_id)
    .bind(policy.hold_seconds as f64)
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


use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::edge_auth::{AuthError, Principal};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Applied,
    Ignored,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub version: u8,
    pub event_id: Uuid,
    pub outcome: Outcome,
    pub reason: Option<String>,
    pub duplicate: bool,
}

#[derive(Debug)]
pub struct Delivery {
    pub ack: Ack,
    pub spot_update: Option<(String, String)>,
    pub unconfirmed_reservation: Option<Uuid>,
}

#[derive(Debug)]
pub struct Observation {
    pub outcome: Outcome,
    pub reason: Option<String>,
    pub spot_update: Option<(String, String)>,
    pub unconfirmed_reservation: Option<Uuid>,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Ignored => "ignored",
            Self::Rejected => "rejected",
        }
    }
}

fn contains_null_character(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains('\0'),
        Value::Array(values) => values.iter().any(contains_null_character),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains('\0') || contains_null_character(value)),
        _ => false,
    }
}

async fn observe(
    tx: &mut Transaction<'_, Postgres>,
    principal: &Principal,
    camera_id: &str,
    spot_id: &str,
    status: &str,
    confidence: f64,
) -> Result<Observation, sqlx::Error> {
    let mut observation = Observation {
        outcome: Outcome::Rejected,
        reason: Some("invalid_status".to_owned()),
        spot_update: None,
        unconfirmed_reservation: None,
    };
    if !crate::edge_auth::spot_allowed(tx, principal, camera_id, spot_id).await? {
        observation.reason = Some("scope_forbidden".to_owned());
        return Ok(observation);
    }
    if status != "free" && status != "occupied" {
        return Ok(observation);
    }
    if confidence < 0.20 {
        observation.outcome = Outcome::Ignored;
        observation.reason = Some("low_confidence".to_owned());
        return Ok(observation);
    }
    let current = sqlx::query_as::<_, (String, bool)>(
        "SELECT status, is_active FROM spots WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(spot_id)
    .fetch_optional(&mut **tx)
    .await?;
    match current {
        None => observation.reason = Some("unknown_spot".to_owned()),
        Some((_, false)) => observation.reason = Some("inactive_spot".to_owned()),
        Some((current, true)) if current == "reserved" && status == "free" => {
            observation.outcome = Outcome::Ignored;
            observation.reason = Some("reservation_protected".to_owned());
        }
        Some(_) => {
            sqlx::query("UPDATE spots SET status = $2, last_updated = NOW() WHERE id = $1")
                .bind(spot_id)
                .bind(status)
                .execute(&mut **tx)
                .await?;
            if status == "free" {
                sqlx::query(
                    "UPDATE user_occupancy_history SET released_at = NOW() \
                     WHERE spot_id = $1 AND released_at IS NULL",
                )
                .bind(spot_id)
                .execute(&mut **tx)
                .await?;
            } else {
                observation.unconfirmed_reservation = sqlx::query_scalar(
                    "SELECT id FROM reservations WHERE spot_id = $1 AND status = 'active'",
                )
                .bind(spot_id)
                .fetch_optional(&mut **tx)
                .await?;
            }
            observation.outcome = Outcome::Applied;
            observation.reason = None;
            observation.spot_update = Some((spot_id.to_owned(), status.to_owned()));
        }
    }
    Ok(observation)
}

pub async fn apply_legacy(
    pool: &PgPool,
    principal: &Principal,
    camera_id: &str,
    spot_id: &str,
    status: &str,
    confidence: f64,
) -> Result<Observation, AuthError> {
    let mut tx = crate::edge_auth::begin_authorized(pool, principal).await?;
    let observation = observe(&mut tx, principal, camera_id, spot_id, status, confidence).await?;
    // Raw observations have no synthetic event UUID, ledger entry or ACK.
    tx.commit().await?;
    Ok(observation)
}

pub async fn apply(
    pool: &PgPool,
    principal: &Principal,
    event_id: Uuid,
    payload: Value,
) -> Result<Delivery, AuthError> {
    let has_null = contains_null_character(&payload);
    // JSONB cannot store NUL. The discriminated wrapper preserves even rejected
    // payloads as escaped JSON text, without colliding with valid payloads.
    let receipt_payload = if has_null {
        serde_json::json!({"json_text": payload.to_string()})
    } else {
        serde_json::json!({"json": &payload})
    };
    let mut tx = crate::edge_auth::begin_authorized(pool, principal).await?;
    // Gateway identity survives credential rotation. The scoped unique ID
    // serializes retries before domain mutations or any ACK can be returned.
    let inserted = sqlx::query(
        "INSERT INTO edge_event_receipts (gateway_id, event_id, payload, outcome) \
         VALUES ($1, $2, $3, 'pending') ON CONFLICT (gateway_id, event_id) DO NOTHING",
    )
    .bind(principal.gateway_id)
    .bind(event_id)
    .bind(sqlx::types::Json(&receipt_payload))
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        let (same_payload, outcome, reason): (bool, String, Option<String>) = sqlx::query_as(
            "SELECT payload = $3, outcome, reason FROM edge_event_receipts \
             WHERE gateway_id = $1 AND event_id = $2",
        )
        .bind(principal.gateway_id)
        .bind(event_id)
        .bind(sqlx::types::Json(&receipt_payload))
        .fetch_one(&mut *tx)
        .await?;
        tx.rollback().await?;
        let outcome = if same_payload {
            match outcome.as_str() {
                "applied" => Outcome::Applied,
                "ignored" => Outcome::Ignored,
                "rejected" => Outcome::Rejected,
                _ => {
                    return Err(
                        sqlx::Error::Protocol("Unfinished delivery receipt".to_owned()).into(),
                    );
                }
            }
        } else {
            Outcome::Rejected
        };
        return Ok(Delivery {
            ack: Ack {
                version: 1,
                event_id,
                outcome,
                reason: if same_payload {
                    reason
                } else {
                    Some("event_id_conflict".to_owned())
                },
                duplicate: same_payload,
            },
            spot_update: None,
            unconfirmed_reservation: None,
        });
    }

    // Historic receipts have no trustworthy producer identity. Never claim
    // them or reapply their UUID; persist a rejection in the new owner's scope.
    let legacy_receipt: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM edge_event_receipts \
         WHERE gateway_id IS NULL AND event_id = $1)",
    )
    .bind(event_id)
    .fetch_one(&mut *tx)
    .await?;
    let mut delivery = Delivery {
        ack: Ack {
            version: 1,
            event_id,
            outcome: Outcome::Rejected,
            reason: Some(
                if legacy_receipt {
                    "legacy_event_receipt"
                } else {
                    "invalid_payload"
                }
                .to_owned(),
            ),
            duplicate: false,
        },
        spot_update: None,
        unconfirmed_reservation: None,
    };
    let parsed_payload = if has_null || legacy_receipt {
        None
    } else {
        serde_json::from_value::<crate::ws::messages::EdgeToServerMsg>(payload).ok()
    };
    match parsed_payload {
        Some(crate::ws::messages::EdgeToServerMsg::SpotUpdate {
            camera_id,
            spot_id,
            status,
            confidence,
            ..
        }) => {
            let observation = observe(
                &mut tx, principal, &camera_id, &spot_id, &status, confidence,
            )
            .await?;
            delivery.ack.outcome = observation.outcome;
            delivery.ack.reason = observation.reason;
            delivery.spot_update = observation.spot_update;
            delivery.unconfirmed_reservation = observation.unconfirmed_reservation;
        }
        Some(_) => delivery.ack.reason = Some("unsupported_event".to_owned()),
        None => {}
    }
    sqlx::query(
        "UPDATE edge_event_receipts SET outcome = $3, reason = $4 \
         WHERE gateway_id = $1 AND event_id = $2",
    )
    .bind(principal.gateway_id)
    .bind(event_id)
    .bind(delivery.ack.outcome.as_str())
    .bind(&delivery.ack.reason)
    .execute(&mut *tx)
    .await?;
    // A receipt and its observation/history become durable together; no success on rollback.
    tx.commit().await?;
    Ok(delivery)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, Utc};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tokio::sync::Barrier;

    use super::*;
    use crate::reservation_store;

    type TestResult = Result<(), AuthError>;

    async fn principal(pool: &PgPool) -> Result<Principal, sqlx::Error> {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Delivery Fixture', $2)",
        )
        .bind(id)
        .bind(Sha256::digest(token.as_bytes()).to_vec())
        .execute(pool)
        .await?;
        Ok(crate::edge_auth::authenticate(pool, &token)
            .await?
            .expect("fixture credential must authenticate"))
    }

    fn payload(spot_id: &str, status: &str) -> Value {
        json!({
            "type": "SPOT_UPDATE",
            "spot_id": spot_id,
            "status": status,
            "camera_id": "test-camera",
            "confidence": 1.0,
            "timestamp": "2026-10-11T12:00:00Z"
        })
    }

    async fn spot(pool: &PgPool, status: &str) -> Result<String, sqlx::Error> {
        let id = format!("delivery-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO spots (id, status, x, y) VALUES ($1, $2, 0, 0)")
            .bind(&id)
            .bind(status)
            .execute(pool)
            .await?;
        Ok(id)
    }

    async fn user(pool: &PgPool) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, name, email, password_hash) \
             VALUES ($1, 'Delivery Fixture', $2, 'unused-test-hash')",
        )
        .bind(id)
        .bind(format!("{id}@example.invalid"))
        .execute(pool)
        .await?;
        Ok(id)
    }

    async fn history(pool: &PgPool, spot_id: &str) -> Result<Uuid, sqlx::Error> {
        let user_id = user(pool).await?;
        sqlx::query_scalar(
            "INSERT INTO user_occupancy_history (user_id, spot_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(spot_id)
        .fetch_one(pool)
        .await
    }

    async fn status(pool: &PgPool, spot_id: &str) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT status FROM spots WHERE id = $1")
            .bind(spot_id)
            .fetch_one(pool)
            .await
    }

    async fn receipt(pool: &PgPool, id: Uuid) -> Result<(String, Option<String>), sqlx::Error> {
        sqlx::query_as("SELECT outcome, reason FROM edge_event_receipts WHERE event_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
    }

    fn assert_no_effects(delivery: &Delivery) {
        assert_eq!(delivery.spot_update, None);
        assert_eq!(delivery.unconfirmed_reservation, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn event_uuid_is_scoped_to_its_authenticated_gateway(pool: PgPool) -> TestResult {
        let first_principal = principal(&pool).await?;
        let second_principal = principal(&pool).await?;
        let first_spot = authorized_spot(&pool, &first_principal, "free").await?;
        let second_spot = authorized_spot(&pool, &second_principal, "free").await?;
        let event_id = Uuid::new_v4();
        let first = apply(
            &pool,
            &first_principal,
            event_id,
            payload(&first_spot, "occupied"),
        )
        .await?;
        let second = apply(
            &pool,
            &second_principal,
            event_id,
            payload(&second_spot, "occupied"),
        )
        .await?;
        assert_eq!(first.ack.outcome, Outcome::Applied);
        assert_eq!(second.ack.outcome, Outcome::Applied);
        assert!(!second.ack.duplicate);
        assert_eq!(status(&pool, &first_spot).await?, "occupied");
        assert_eq!(status(&pool, &second_spot).await?, "occupied");
        let owners: Vec<Uuid> = sqlx::query_scalar(
            "SELECT gateway_id FROM edge_event_receipts WHERE event_id = $1 ORDER BY gateway_id",
        )
        .bind(event_id)
        .fetch_all(&pool)
        .await?;
        let mut expected = vec![first_principal.gateway_id, second_principal.gateway_id];
        expected.sort();
        assert_eq!(owners, expected);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn legacy_uuid_is_rejected_without_claiming_or_reapplying_the_receipt(
        pool: PgPool,
    ) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let event_id = Uuid::new_v4();
        let event_payload = payload(&spot_id, "occupied");
        let stored_payload = json!({"json": &event_payload});
        sqlx::query(
            "INSERT INTO edge_event_receipts (event_id, payload, outcome) VALUES ($1, $2, 'applied')",
        )
        .bind(event_id)
        .bind(sqlx::types::Json(&stored_payload))
        .execute(&pool)
        .await?;
        let first = apply(&pool, &principal, event_id, event_payload.clone()).await?;
        assert_eq!(first.ack.outcome, Outcome::Rejected);
        assert_eq!(first.ack.reason.as_deref(), Some("legacy_event_receipt"));
        assert!(!first.ack.duplicate);
        assert_no_effects(&first);
        assert_eq!(status(&pool, &spot_id).await?, "free");
        let legacy_preserved: bool = sqlx::query_scalar(
            "SELECT payload = $2 AND outcome = 'applied' AND reason IS NULL \
             FROM edge_event_receipts WHERE event_id = $1 AND gateway_id IS NULL",
        )
        .bind(event_id)
        .bind(sqlx::types::Json(&stored_payload))
        .fetch_one(&pool)
        .await?;
        assert!(legacy_preserved);
        let own: (String, Option<String>) = sqlx::query_as(
            "SELECT outcome, reason FROM edge_event_receipts WHERE event_id = $1 AND gateway_id = $2",
        )
        .bind(event_id)
        .bind(principal.gateway_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            own,
            (
                "rejected".to_owned(),
                Some("legacy_event_receipt".to_owned())
            )
        );
        let duplicate = apply(&pool, &principal, event_id, event_payload).await?;
        assert!(duplicate.ack.duplicate);
        assert_eq!(
            duplicate.ack.reason.as_deref(),
            Some("legacy_event_receipt")
        );
        assert_no_effects(&duplicate);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn revoked_credential_cannot_replay_or_create_a_receipt(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let event_id = Uuid::new_v4();
        let event_payload = payload(&spot_id, "occupied");
        apply(&pool, &principal, event_id, event_payload.clone()).await?;
        sqlx::query("UPDATE edge_gateways SET revoked_at = NOW() WHERE id = $1")
            .bind(principal.gateway_id)
            .execute(&pool)
            .await?;
        assert!(matches!(
            apply(&pool, &principal, event_id, event_payload).await,
            Err(AuthError::Unauthenticated)
        ));
        assert!(matches!(
            apply(&pool, &principal, Uuid::new_v4(), payload(&spot_id, "free")).await,
            Err(AuthError::Unauthenticated)
        ));
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn rotated_credential_replays_the_same_gateway_receipt(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let event_id = Uuid::new_v4();
        let event_payload = payload(&spot_id, "occupied");
        apply(&pool, &principal, event_id, event_payload.clone()).await?;
        sqlx::query("UPDATE spots SET status = 'free' WHERE id = $1")
            .bind(&spot_id)
            .execute(&pool)
            .await?;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        sqlx::query("UPDATE edge_gateways SET credential_digest = $2 WHERE id = $1")
            .bind(principal.gateway_id)
            .bind(Sha256::digest(token.as_bytes()).to_vec())
            .execute(&pool)
            .await?;
        let rotated = crate::edge_auth::authenticate(&pool, &token)
            .await?
            .expect("rotated credential must authenticate");
        assert_eq!(rotated.gateway_id, principal.gateway_id);
        let duplicate = apply(&pool, &rotated, event_id, event_payload).await?;
        assert_eq!(duplicate.ack.outcome, Outcome::Applied);
        assert!(duplicate.ack.duplicate);
        assert_no_effects(&duplicate);
        assert_eq!(status(&pool, &spot_id).await?, "free");
        assert!(matches!(
            apply(&pool, &principal, event_id, payload(&spot_id, "occupied")).await,
            Err(AuthError::Unauthenticated)
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn occupied_receipt_and_captured_reservation_are_committed(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let owner = user(&pool).await?;
        let reservation =
            reservation_store::create(&pool, &spot_id, owner, Utc::now() + Duration::minutes(15))
                .await
                .expect("fixture reservation must be created");
        let id = Uuid::new_v4();
        let delivery = apply(&pool, &principal, id, payload(&spot_id, "occupied")).await?;

        assert_eq!(delivery.ack.outcome, Outcome::Applied);
        assert_eq!(delivery.ack.reason, None);
        assert!(!delivery.ack.duplicate);
        assert_eq!(delivery.unconfirmed_reservation, Some(reservation.id));
        assert_eq!(
            delivery.spot_update,
            Some((spot_id.clone(), "occupied".to_owned()))
        );
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        assert_eq!(receipt(&pool, id).await?, ("applied".to_owned(), None));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn duplicate_returns_original_ack_without_reapplying(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let id = Uuid::new_v4();
        let first_payload = payload(&spot_id, "occupied");
        let first = apply(&pool, &principal, id, first_payload.clone()).await?;
        assert_eq!(first.ack.outcome, Outcome::Applied);
        sqlx::query("UPDATE spots SET status = 'free' WHERE id = $1")
            .bind(&spot_id)
            .execute(&pool)
            .await?;

        // JSONB equality also considers 1 and 1.0 equal, independently of key order.
        let mut canonical_duplicate = first_payload;
        canonical_duplicate["confidence"] = json!(1);
        let duplicate = apply(&pool, &principal, id, canonical_duplicate).await?;
        assert_eq!(duplicate.ack.outcome, first.ack.outcome);
        assert_eq!(duplicate.ack.reason, first.ack.reason);
        assert!(duplicate.ack.duplicate);
        assert_no_effects(&duplicate);
        assert_eq!(status(&pool, &spot_id).await?, "free");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_duplicate_mutates_spot_once(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        sqlx::raw_sql(
            "CREATE TABLE delivery_effect_count (updates INTEGER NOT NULL); \
             INSERT INTO delivery_effect_count VALUES (0); \
             CREATE FUNCTION count_delivery_update() RETURNS TRIGGER LANGUAGE plpgsql AS $$ \
             BEGIN UPDATE delivery_effect_count SET updates = updates + 1; RETURN NEW; END $$; \
             CREATE TRIGGER count_delivery_update BEFORE UPDATE ON spots \
             FOR EACH ROW EXECUTE FUNCTION count_delivery_update();",
        )
        .execute(&pool)
        .await?;
        let id = Uuid::new_v4();
        let event_payload = payload(&spot_id, "occupied");
        let barrier = Arc::new(Barrier::new(2));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let pool = pool.clone();
            let principal = principal.clone();
            let payload = event_payload.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                apply(&pool, &principal, id, payload).await
            }));
        }
        let first = tasks
            .remove(0)
            .await
            .expect("first worker must not panic")?;
        let second = tasks
            .remove(0)
            .await
            .expect("second worker must not panic")?;
        assert_eq!(first.ack.outcome, Outcome::Applied);
        assert_eq!(second.ack.outcome, Outcome::Applied);
        assert_ne!(first.ack.duplicate, second.ack.duplicate);
        let duplicate = if first.ack.duplicate { &first } else { &second };
        assert_no_effects(duplicate);
        let updates: i32 = sqlx::query_scalar("SELECT updates FROM delivery_effect_count")
            .fetch_one(&pool)
            .await?;
        assert_eq!(updates, 1);
        assert_eq!(receipt(&pool, id).await?, ("applied".to_owned(), None));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn reused_id_rejects_changed_payload_and_preserves_original(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let id = Uuid::new_v4();
        let original = payload(&spot_id, "occupied");
        assert_eq!(
            apply(&pool, &principal, id, original.clone())
                .await?
                .ack
                .outcome,
            Outcome::Applied
        );

        let conflict = apply(&pool, &principal, id, payload(&spot_id, "free")).await?;
        assert_eq!(conflict.ack.outcome, Outcome::Rejected);
        assert_eq!(conflict.ack.reason.as_deref(), Some("event_id_conflict"));
        assert!(!conflict.ack.duplicate);
        assert_no_effects(&conflict);
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        let preserved: bool = sqlx::query_scalar(
            "SELECT payload->'json' = $2 AND outcome = 'applied' AND reason IS NULL \
             FROM edge_event_receipts WHERE event_id = $1",
        )
        .bind(id)
        .bind(sqlx::types::Json(original.clone()))
        .fetch_one(&pool)
        .await?;
        assert!(preserved);
        assert!(apply(&pool, &principal, id, original).await?.ack.duplicate);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn free_event_releases_history_and_commits_receipt(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "occupied").await?;
        let history_id = history(&pool, &spot_id).await?;
        let id = Uuid::new_v4();
        let delivery = apply(&pool, &principal, id, payload(&spot_id, "free")).await?;
        assert_eq!(delivery.ack.outcome, Outcome::Applied);
        assert_eq!(
            delivery.spot_update,
            Some((spot_id.clone(), "free".to_owned()))
        );
        assert_eq!(delivery.unconfirmed_reservation, None);
        assert_eq!(status(&pool, &spot_id).await?, "free");
        let released: bool = sqlx::query_scalar(
            "SELECT released_at IS NOT NULL FROM user_occupancy_history WHERE id = $1",
        )
        .bind(history_id)
        .fetch_one(&pool)
        .await?;
        assert!(released);
        assert_eq!(receipt(&pool, id).await?, ("applied".to_owned(), None));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn finalization_error_rolls_back_receipt_spot_and_history(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "occupied").await?;
        let history_id = history(&pool, &spot_id).await?;
        sqlx::raw_sql(
            "CREATE FUNCTION fail_delivery_receipt() RETURNS TRIGGER LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'synthetic receipt failure'; END $$; \
             CREATE TRIGGER fail_delivery_receipt BEFORE UPDATE ON edge_event_receipts \
             FOR EACH ROW EXECUTE FUNCTION fail_delivery_receipt();",
        )
        .execute(&pool)
        .await?;
        let id = Uuid::new_v4();
        let event_payload = payload(&spot_id, "free");
        assert!(
            apply(&pool, &principal, id, event_payload.clone())
                .await
                .is_err()
        );
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        let unchanged: bool = sqlx::query_scalar(
            "SELECT released_at IS NULL FROM user_occupancy_history WHERE id = $1",
        )
        .bind(history_id)
        .fetch_one(&pool)
        .await?;
        assert!(unchanged);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts WHERE event_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 0);
        sqlx::query("DROP TRIGGER fail_delivery_receipt ON edge_event_receipts")
            .execute(&pool)
            .await?;
        let retry = apply(&pool, &principal, id, event_payload).await?;
        assert_eq!(retry.ack.outcome, Outcome::Applied);
        assert!(!retry.ack.duplicate);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ignored_events_preserve_spot_and_history_and_are_deduplicated(
        pool: PgPool,
    ) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "reserved").await?;
        let history_id = history(&pool, &spot_id).await?;
        let mut low_confidence = payload(&spot_id, "occupied");
        low_confidence["confidence"] = json!(0.19);
        let protected = payload(&spot_id, "free");
        for (event_payload, reason) in [
            (low_confidence, "low_confidence"),
            (protected, "reservation_protected"),
        ] {
            let id = Uuid::new_v4();
            let first = apply(&pool, &principal, id, event_payload.clone()).await?;
            assert_eq!(first.ack.outcome, Outcome::Ignored);
            assert_eq!(first.ack.reason.as_deref(), Some(reason));
            assert!(!first.ack.duplicate);
            assert_no_effects(&first);
            let duplicate = apply(&pool, &principal, id, event_payload).await?;
            assert_eq!(duplicate.ack.outcome, Outcome::Ignored);
            assert_eq!(duplicate.ack.reason, first.ack.reason);
            assert!(duplicate.ack.duplicate);
            assert_no_effects(&duplicate);
            assert_eq!(
                receipt(&pool, id).await?,
                ("ignored".to_owned(), Some(reason.to_owned()))
            );
        }
        assert_eq!(status(&pool, &spot_id).await?, "reserved");
        let unreleased: bool = sqlx::query_scalar(
            "SELECT released_at IS NULL FROM user_occupancy_history WHERE id = $1",
        )
        .bind(history_id)
        .fetch_one(&pool)
        .await?;
        assert!(unreleased);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn invalid_and_unsupported_payloads_have_terminal_receipts(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let mut missing = payload(&spot_id, "occupied");
        missing.as_object_mut().unwrap().remove("camera_id");
        let mut null_character = payload(&spot_id, "free");
        null_character["camera_id"] = json!("cam\0");
        let mut null_key = payload(&spot_id, "free");
        null_key["key\0"] = json!("value");
        let mut nested_null = payload(&spot_id, "free");
        nested_null["extra"] = json!([{"name": "nested\0"}]);
        let cases = [
            (missing, "invalid_payload"),
            (null_character, "invalid_payload"),
            (null_key, "invalid_payload"),
            (nested_null, "invalid_payload"),
            (payload(&spot_id, "reserved"), "invalid_status"),
            (payload("unknown-delivery-spot", "free"), "scope_forbidden"),
            (
                json!({"type": "PATH_UPDATE", "from_node": "a", "to_node": "b", "is_active": true}),
                "unsupported_event",
            ),
        ];
        for (event_payload, reason) in cases {
            let id = Uuid::new_v4();
            let first = apply(&pool, &principal, id, event_payload.clone()).await?;
            assert_eq!(first.ack.outcome, Outcome::Rejected);
            assert_eq!(first.ack.reason.as_deref(), Some(reason));
            assert_no_effects(&first);
            assert_eq!(
                receipt(&pool, id).await?,
                ("rejected".to_owned(), Some(reason.to_owned()))
            );
            let duplicate = apply(&pool, &principal, id, event_payload).await?;
            assert_eq!(duplicate.ack.outcome, Outcome::Rejected);
            assert_eq!(duplicate.ack.reason, first.ack.reason);
            assert!(duplicate.ack.duplicate);
            assert_no_effects(&duplicate);
        }
        assert_eq!(status(&pool, &spot_id).await?, "free");
        Ok(())
    }

    async fn wait_for_blocked_query(pool: &PgPool, fragment: &str) -> Result<(), sqlx::Error> {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let blocked: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                     WHERE datname = current_database() AND pid <> pg_backend_pid() \
                     AND wait_event_type = 'Lock' AND position($1 in query) > 0)",
                )
                .bind(fragment)
                .fetch_one(pool)
                .await?;
                if blocked {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("expected statement must wait for a database lock")
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn authorized_delivery_holds_revocation_until_commit(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT id FROM spots WHERE id = $1 FOR UPDATE")
            .bind(&spot_id)
            .execute(&mut *blocker)
            .await?;
        let delivery_pool = pool.clone();
        let delivery_principal = principal.clone();
        let event_payload = payload(&spot_id, "occupied");
        let delivery = tokio::spawn(async move {
            apply(
                &delivery_pool,
                &delivery_principal,
                Uuid::new_v4(),
                event_payload,
            )
            .await
        });
        wait_for_blocked_query(&pool, "SELECT status, is_active FROM spots").await?;
        let revocation_pool = pool.clone();
        let gateway_id = principal.gateway_id;
        let revocation = tokio::spawn(async move {
            sqlx::query("UPDATE edge_gateways SET revoked_at = NOW() WHERE id = $1")
                .bind(gateway_id)
                .execute(&revocation_pool)
                .await
        });
        wait_for_blocked_query(&pool, "UPDATE edge_gateways SET revoked_at").await?;
        assert!(!revocation.is_finished());
        blocker.commit().await?;
        let committed = tokio::time::timeout(std::time::Duration::from_secs(3), delivery)
            .await
            .expect("delivery must complete after releasing the spot")
            .expect("delivery task must not panic")?;
        assert_eq!(committed.ack.outcome, Outcome::Applied);
        tokio::time::timeout(std::time::Duration::from_secs(1), revocation)
            .await
            .expect("revocation must complete after delivery commit")
            .expect("revocation task must not panic")?;
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        assert!(matches!(
            apply(&pool, &principal, Uuid::new_v4(), payload(&spot_id, "free")).await,
            Err(AuthError::Unauthenticated)
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn observation_lock_timeout_rolls_back_the_pending_receipt(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "occupied").await?;
        let history_id = history(&pool, &spot_id).await?;
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT id FROM spots WHERE id = $1 FOR UPDATE")
            .bind(&spot_id)
            .execute(&mut *blocker)
            .await?;
        let event_id = Uuid::new_v4();
        let event_payload = payload(&spot_id, "free");
        let failure = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            apply(&pool, &principal, event_id, event_payload.clone()),
        )
        .await
        .expect("database deadline must bound the observation wait");
        let Err(AuthError::Database(error)) = failure else {
            panic!("blocked observation must return a database timeout");
        };
        let code = error.as_database_error().and_then(|error| error.code());
        assert!(matches!(code.as_deref(), Some("55P03" | "57014")));
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts WHERE event_id = $1")
                .bind(event_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 0);
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        let unreleased: bool = sqlx::query_scalar(
            "SELECT released_at IS NULL FROM user_occupancy_history WHERE id = $1",
        )
        .bind(history_id)
        .fetch_one(&pool)
        .await?;
        assert!(unreleased);
        blocker.rollback().await?;
        let retry = apply(&pool, &principal, event_id, event_payload).await?;
        assert_eq!(retry.ack.outcome, Outcome::Applied);
        assert!(!retry.ack.duplicate);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ownership_upgrade_preserves_unattributed_receipts(pool: PgPool) -> TestResult {
        sqlx::raw_sql(include_str!(
            "../migrations/20261012020000_edge_receipt_ownership.down.sql"
        ))
        .execute(&pool)
        .await?;
        let event_id = Uuid::new_v4();
        let stored_payload = json!({"json": {"historic": true}});
        sqlx::query(
            "INSERT INTO edge_event_receipts (event_id, payload, outcome, reason, created_at) \
             VALUES ($1, $2, 'ignored', 'low_confidence', '2026-10-11T12:00:00Z')",
        )
        .bind(event_id)
        .bind(sqlx::types::Json(&stored_payload))
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/20261012020000_edge_receipt_ownership.up.sql"
        ))
        .execute(&pool)
        .await?;
        let preserved: bool = sqlx::query_scalar(
            "SELECT gateway_id IS NULL AND payload = $2 AND outcome = 'ignored' \
             AND reason = 'low_confidence' AND created_at = '2026-10-11T12:00:00Z' \
             FROM edge_event_receipts WHERE event_id = $1",
        )
        .bind(event_id)
        .bind(sqlx::types::Json(&stored_payload))
        .fetch_one(&pool)
        .await?;
        assert!(preserved);
        // Legacy uniqueness remains enforced without fabricating an owner.
        assert!(
            sqlx::query(
                "INSERT INTO edge_event_receipts (event_id, payload, outcome) \
                 VALUES ($1, '{}', 'applied')"
            )
            .bind(event_id)
            .execute(&pool)
            .await
            .is_err()
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ambiguous_ownership_downgrade_preserves_every_receipt(pool: PgPool) -> TestResult {
        let first_principal = principal(&pool).await?;
        let second_principal = principal(&pool).await?;
        let event_id = Uuid::new_v4();
        for principal in [&first_principal, &second_principal] {
            sqlx::query(
                "INSERT INTO edge_event_receipts (gateway_id, event_id, payload, outcome) \
                 VALUES ($1, $2, '{}', 'rejected')",
            )
            .bind(principal.gateway_id)
            .bind(event_id)
            .execute(&pool)
            .await?;
        }
        assert!(
            sqlx::raw_sql(include_str!(
                "../migrations/20261012020000_edge_receipt_ownership.down.sql"
            ))
            .execute(&pool)
            .await
            .is_err()
        );
        let owners: Vec<Uuid> = sqlx::query_scalar(
            "SELECT gateway_id FROM edge_event_receipts WHERE event_id = $1 ORDER BY gateway_id",
        )
        .bind(event_id)
        .fetch_all(&pool)
        .await?;
        let mut expected = vec![first_principal.gateway_id, second_principal.gateway_id];
        expected.sort();
        assert_eq!(owners, expected);
        Ok(())
    }

    async fn authorized_spot(
        pool: &PgPool,
        principal: &Principal,
        status: &str,
    ) -> Result<String, sqlx::Error> {
        let spot_id = spot(pool, status).await?;
        sqlx::query(
            "INSERT INTO edge_camera_grants (gateway_id, camera_id, spot_update, car_detected) \
             VALUES ($1, 'test-camera', TRUE, FALSE) \
             ON CONFLICT (gateway_id, camera_id) DO UPDATE SET spot_update = TRUE",
        )
        .bind(principal.gateway_id)
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO edge_camera_spots (gateway_id, camera_id, spot_id) \
             VALUES ($1, 'test-camera', $2)",
        )
        .bind(principal.gateway_id)
        .bind(&spot_id)
        .execute(pool)
        .await?;
        Ok(spot_id)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn forbidden_durable_and_legacy_observations_preserve_domain_state(
        pool: PgPool,
    ) -> TestResult {
        let principal = principal(&pool).await?;
        let own_spot = authorized_spot(&pool, &principal, "occupied").await?;
        let foreign_spot = spot(&pool, "occupied").await?;
        let history_id = history(&pool, &foreign_spot).await?;
        for (camera_id, spot_id) in [
            ("test-camera", foreign_spot.as_str()),
            ("forged-camera", own_spot.as_str()),
        ] {
            let mut event_payload = payload(spot_id, "free");
            event_payload["camera_id"] = json!(camera_id);
            let delivery = apply(&pool, &principal, Uuid::new_v4(), event_payload).await?;
            assert_eq!(delivery.ack.outcome, Outcome::Rejected);
            assert_eq!(delivery.ack.reason.as_deref(), Some("scope_forbidden"));
            assert_no_effects(&delivery);
            let legacy = apply_legacy(&pool, &principal, camera_id, spot_id, "free", 1.0).await?;
            assert_eq!(legacy.outcome, Outcome::Rejected);
            assert_eq!(legacy.reason.as_deref(), Some("scope_forbidden"));
            assert_eq!(legacy.spot_update, None);
            assert_eq!(legacy.unconfirmed_reservation, None);
        }
        assert_eq!(status(&pool, &own_spot).await?, "occupied");
        assert_eq!(status(&pool, &foreign_spot).await?, "occupied");
        let unreleased: bool = sqlx::query_scalar(
            "SELECT released_at IS NULL FROM user_occupancy_history WHERE id = $1",
        )
        .bind(history_id)
        .fetch_one(&pool)
        .await?;
        assert!(unreleased);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 2, "legacy observations must not fabricate receipts");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn foreign_gateway_reusing_uuid_cannot_read_the_owner_ack(pool: PgPool) -> TestResult {
        let owner = principal(&pool).await?;
        let attacker = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &owner, "free").await?;
        let event_id = Uuid::new_v4();
        let event_payload = payload(&spot_id, "occupied");
        let original = apply(&pool, &owner, event_id, event_payload.clone()).await?;
        assert_eq!(original.ack.outcome, Outcome::Applied);
        let forbidden = apply(&pool, &attacker, event_id, event_payload.clone()).await?;
        assert_eq!(forbidden.ack.outcome, Outcome::Rejected);
        assert_eq!(forbidden.ack.reason.as_deref(), Some("scope_forbidden"));
        assert!(!forbidden.ack.duplicate);
        assert_no_effects(&forbidden);
        let original_replay = apply(&pool, &owner, event_id, event_payload).await?;
        assert_eq!(original_replay.ack.outcome, Outcome::Applied);
        assert!(original_replay.ack.duplicate);
        assert_no_effects(&original_replay);
        let outcomes: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT gateway_id, outcome FROM edge_event_receipts WHERE event_id = $1",
        )
        .bind(event_id)
        .fetch_all(&pool)
        .await?;
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.contains(&(owner.gateway_id, "applied".to_owned())));
        assert!(outcomes.contains(&(attacker.gateway_id, "rejected".to_owned())));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn legacy_observation_denies_a_foreign_spot_without_a_receipt(
        pool: PgPool,
    ) -> TestResult {
        let principal = principal(&pool).await?;
        authorized_spot(&pool, &principal, "free").await?;
        let foreign_spot = spot(&pool, "occupied").await?;
        let history_id = history(&pool, &foreign_spot).await?;
        let legacy =
            apply_legacy(&pool, &principal, "test-camera", &foreign_spot, "free", 1.0).await?;
        assert_eq!(legacy.outcome, Outcome::Rejected);
        assert_eq!(legacy.reason.as_deref(), Some("scope_forbidden"));
        assert_eq!(legacy.spot_update, None);
        assert_eq!(legacy.unconfirmed_reservation, None);
        assert_eq!(status(&pool, &foreign_spot).await?, "occupied");
        let unreleased: bool = sqlx::query_scalar(
            "SELECT released_at IS NULL FROM user_occupancy_history WHERE id = $1",
        )
        .bind(history_id)
        .fetch_one(&pool)
        .await?;
        assert!(unreleased);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn exact_pair_still_requires_spot_update_permission(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        sqlx::query("UPDATE edge_camera_grants SET spot_update = FALSE WHERE gateway_id = $1")
            .bind(principal.gateway_id)
            .execute(&pool)
            .await?;
        let delivery = apply(
            &pool,
            &principal,
            Uuid::new_v4(),
            payload(&spot_id, "occupied"),
        )
        .await?;
        assert_eq!(delivery.ack.outcome, Outcome::Rejected);
        assert_eq!(delivery.ack.reason.as_deref(), Some("scope_forbidden"));
        assert_no_effects(&delivery);
        let legacy =
            apply_legacy(&pool, &principal, "test-camera", &spot_id, "occupied", 1.0).await?;
        assert_eq!(legacy.outcome, Outcome::Rejected);
        assert_eq!(legacy.reason.as_deref(), Some("scope_forbidden"));
        assert_eq!(legacy.spot_update, None);
        assert_eq!(status(&pool, &spot_id).await?, "free");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn removed_grant_preserves_historical_ack_but_denies_new_observations(
        pool: PgPool,
    ) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let event_id = Uuid::new_v4();
        let event_payload = payload(&spot_id, "occupied");
        apply(&pool, &principal, event_id, event_payload.clone()).await?;
        sqlx::query("DELETE FROM edge_camera_spots WHERE gateway_id = $1")
            .bind(principal.gateway_id)
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE spots SET status = 'free' WHERE id = $1")
            .bind(&spot_id)
            .execute(&pool)
            .await?;
        let duplicate = apply(&pool, &principal, event_id, event_payload.clone()).await?;
        assert_eq!(duplicate.ack.outcome, Outcome::Applied);
        assert!(duplicate.ack.duplicate);
        assert_no_effects(&duplicate);
        let new_event = apply(&pool, &principal, Uuid::new_v4(), event_payload).await?;
        assert_eq!(new_event.ack.outcome, Outcome::Rejected);
        assert_eq!(new_event.ack.reason.as_deref(), Some("scope_forbidden"));
        let legacy =
            apply_legacy(&pool, &principal, "test-camera", &spot_id, "occupied", 1.0).await?;
        assert_eq!(legacy.outcome, Outcome::Rejected);
        assert_eq!(legacy.reason.as_deref(), Some("scope_forbidden"));
        assert_eq!(status(&pool, &spot_id).await?, "free");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn legacy_authorized_observation_commits_without_a_receipt(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let owner = user(&pool).await?;
        let reservation =
            reservation_store::create(&pool, &spot_id, owner, Utc::now() + Duration::minutes(15))
                .await
                .expect("fixture reservation must be created");
        let occupied =
            apply_legacy(&pool, &principal, "test-camera", &spot_id, "occupied", 1.0).await?;
        assert_eq!(occupied.outcome, Outcome::Applied);
        assert_eq!(occupied.unconfirmed_reservation, Some(reservation.id));
        assert_eq!(
            occupied.spot_update,
            Some((spot_id.clone(), "occupied".to_owned()))
        );
        let history_id = history(&pool, &spot_id).await?;
        let free = apply_legacy(&pool, &principal, "test-camera", &spot_id, "free", 1.0).await?;
        assert_eq!(free.outcome, Outcome::Applied);
        assert_eq!(status(&pool, &spot_id).await?, "free");
        let released: bool = sqlx::query_scalar(
            "SELECT released_at IS NOT NULL FROM user_occupancy_history WHERE id = $1",
        )
        .bind(history_id)
        .fetch_one(&pool)
        .await?;
        assert!(released);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        sqlx::query("UPDATE edge_gateways SET revoked_at = NOW() WHERE id = $1")
            .bind(principal.gateway_id)
            .execute(&pool)
            .await?;
        assert!(matches!(
            apply_legacy(&pool, &principal, "test-camera", &spot_id, "occupied", 1.0).await,
            Err(AuthError::Unauthenticated)
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn legacy_history_failure_rolls_back_the_spot_without_a_receipt(
        pool: PgPool,
    ) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "occupied").await?;
        let history_id = history(&pool, &spot_id).await?;
        sqlx::raw_sql(
            "CREATE FUNCTION fail_legacy_history() RETURNS TRIGGER LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'synthetic history failure'; END $$; \
         CREATE TRIGGER fail_legacy_history BEFORE UPDATE ON user_occupancy_history \
         FOR EACH ROW EXECUTE FUNCTION fail_legacy_history();",
        )
        .execute(&pool)
        .await?;
        assert!(matches!(
            apply_legacy(&pool, &principal, "test-camera", &spot_id, "free", 1.0).await,
            Err(AuthError::Database(_))
        ));
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        let unreleased: bool = sqlx::query_scalar(
            "SELECT released_at IS NULL FROM user_occupancy_history WHERE id = $1",
        )
        .bind(history_id)
        .fetch_one(&pool)
        .await?;
        assert!(unreleased);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edge_event_receipts")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        Ok(())
    }
    // R01 + R02 + R05: a replay must not undo a newer owner's reservation.
    #[sqlx::test(migrations = "./migrations")]
    async fn replayed_free_event_preserves_a_new_reservation(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "occupied").await?;
        let event_id = Uuid::new_v4();
        let frame = payload(&spot_id, "free");
        let first = apply(&pool, &principal, event_id, frame.clone()).await?;
        assert_eq!(first.ack.outcome, Outcome::Applied);
        let owner = user(&pool).await?;
        let reservation = reservation_store::create(
            &pool,
            &spot_id,
            owner,
            Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .expect("new owner reserves the released spot");
        let replay = apply(&pool, &principal, event_id, frame).await?;
        assert!(replay.ack.duplicate);
        assert_eq!(replay.ack.outcome, Outcome::Applied);
        assert!(replay.spot_update.is_none());
        let fresh = apply(&pool, &principal, Uuid::new_v4(), payload(&spot_id, "free")).await?;
        assert_eq!(fresh.ack.outcome, Outcome::Ignored);
        assert_eq!(fresh.ack.reason.as_deref(), Some("reservation_protected"));
        assert_eq!(status(&pool, &spot_id).await?, "reserved");
        let claim: (Uuid, String) =
            sqlx::query_as("SELECT user_id, status FROM reservations WHERE id = $1")
                .bind(reservation.id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(claim, (owner, "active".to_owned()));
        Ok(())
    }

    // The authority lock must protect policy replacement as well as revocation.
    #[sqlx::test(migrations = "./migrations")]
    async fn grant_removal_waits_for_delivery_then_rejects_new_events(pool: PgPool) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT id FROM spots WHERE id = $1 FOR UPDATE")
            .bind(&spot_id)
            .execute(&mut *blocker)
            .await?;
        let event_id = Uuid::new_v4();
        let frame = payload(&spot_id, "occupied");
        let worker_pool = pool.clone();
        let worker_principal = principal.clone();
        let worker_frame = frame.clone();
        let delivery = tokio::spawn(async move {
            apply(&worker_pool, &worker_principal, event_id, worker_frame).await
        });
        wait_for_blocked_query(&pool, "SELECT status, is_active FROM spots").await?;
        let policy_pool = pool.clone();
        let gateway_id = principal.gateway_id;
        let policy_path = std::env::temp_dir().join(format!("p1-policy-{}.json", Uuid::new_v4()));
        std::fs::write(
            &policy_path,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[]}"#,
        )
        .expect("write disposable policy");
        let args = vec![
            "server".to_owned(),
            "--edge-policy".to_owned(),
            gateway_id.to_string(),
            policy_path.to_string_lossy().into_owned(),
        ];
        let policy =
            tokio::spawn(async move { crate::edge_cli::dispatch(&policy_pool, &args).await });
        wait_for_blocked_query(&pool, "FROM edge_gateways").await?;
        assert!(!policy.is_finished());
        blocker.commit().await?;
        assert_eq!(
            delivery
                .await
                .expect("delivery worker")
                .unwrap()
                .ack
                .outcome,
            Outcome::Applied
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), policy)
            .await
            .expect("policy must complete")
            .expect("policy worker")
            .expect("CLI applies policy");
        std::fs::remove_file(policy_path).expect("remove disposable policy");
        let replay = apply(&pool, &principal, event_id, frame).await?;
        assert!(replay.ack.duplicate);
        assert_eq!(replay.ack.outcome, Outcome::Applied);
        let fresh = apply(&pool, &principal, Uuid::new_v4(), payload(&spot_id, "free")).await?;
        assert_eq!(fresh.ack.reason.as_deref(), Some("scope_forbidden"));
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        Ok(())
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn free_observation_waits_for_reservation_and_preserves_its_owner(
        pool: PgPool,
    ) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let owner = user(&pool).await?;
        let mut tx = pool.begin().await?;
        let reservation = reservation_store::create_in_transaction(
            &mut tx,
            &spot_id,
            owner,
            Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .expect("reservation claims free spot inside transaction");
        let worker_pool = pool.clone();
        let frame = payload(&spot_id, "free");
        let delivery =
            tokio::spawn(
                async move { apply(&worker_pool, &principal, Uuid::new_v4(), frame).await },
            );
        wait_for_blocked_query(&pool, "SELECT status, is_active FROM spots").await?;
        assert!(!delivery.is_finished());
        tx.commit().await?;
        let observed = tokio::time::timeout(std::time::Duration::from_secs(3), delivery)
            .await
            .expect("delivery completes after reservation commit")
            .expect("delivery worker")?;
        assert_eq!(observed.ack.outcome, Outcome::Ignored);
        assert_eq!(
            observed.ack.reason.as_deref(),
            Some("reservation_protected")
        );
        assert!(observed.spot_update.is_none());
        assert_eq!(status(&pool, &spot_id).await?, "reserved");
        let claim: (Uuid, String) =
            sqlx::query_as("SELECT user_id,status FROM reservations WHERE id=$1")
                .bind(reservation.id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(claim, (owner, "active".to_owned()));
        Ok(())
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn retired_spot_rejects_new_observations_but_preserves_historical_ack(
        pool: PgPool,
    ) -> TestResult {
        let principal = principal(&pool).await?;
        let spot_id = authorized_spot(&pool, &principal, "free").await?;
        let event_id = Uuid::new_v4();
        let frame = payload(&spot_id, "occupied");
        let first = apply(&pool, &principal, event_id, frame.clone()).await?;
        assert_eq!(first.ack.outcome, Outcome::Applied);
        sqlx::query("UPDATE spots SET is_active=FALSE WHERE id=$1")
            .bind(&spot_id)
            .execute(&pool)
            .await?;
        let replay = apply(&pool, &principal, event_id, frame).await?;
        assert_eq!(replay.ack.outcome, Outcome::Applied);
        assert!(replay.ack.duplicate);
        assert!(replay.spot_update.is_none());
        let fresh = apply(&pool, &principal, Uuid::new_v4(), payload(&spot_id, "free")).await?;
        assert_eq!(fresh.ack.outcome, Outcome::Rejected);
        assert_eq!(fresh.ack.reason.as_deref(), Some("inactive_spot"));
        let legacy = apply_legacy(&pool, &principal, "test-camera", &spot_id, "free", 1.0).await?;
        assert_eq!(legacy.reason.as_deref(), Some("inactive_spot"));
        assert!(legacy.spot_update.is_none());
        assert_eq!(status(&pool, &spot_id).await?, "occupied");
        Ok(())
    }
}

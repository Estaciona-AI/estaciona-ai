use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

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

pub async fn apply(pool: &PgPool, event_id: Uuid, payload: Value) -> Result<Delivery, sqlx::Error> {
    let has_null = contains_null_character(&payload);
    // JSONB cannot store NUL. The discriminated wrapper preserves even rejected
    // payloads as escaped JSON text, without colliding with valid payloads.
    let receipt_payload = if has_null {
        serde_json::json!({"json_text": payload.to_string()})
    } else {
        serde_json::json!({"json": &payload})
    };
    let mut tx = pool.begin().await?;
    // The unique event ID serializes concurrent retries before any domain mutation.
    let inserted = sqlx::query(
        "INSERT INTO edge_event_receipts (event_id, payload, outcome) \
         VALUES ($1, $2, 'pending') ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event_id)
    .bind(sqlx::types::Json(&receipt_payload))
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        let (same_payload, outcome, reason): (bool, String, Option<String>) = sqlx::query_as(
            "SELECT payload = $2, outcome, reason FROM edge_event_receipts WHERE event_id = $1",
        )
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
                    return Err(sqlx::Error::Protocol(
                        "Unfinished delivery receipt".to_owned(),
                    ));
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

    let mut delivery = Delivery {
        ack: Ack {
            version: 1,
            event_id,
            outcome: Outcome::Rejected,
            reason: Some("invalid_payload".to_owned()),
            duplicate: false,
        },
        spot_update: None,
        unconfirmed_reservation: None,
    };
    let parsed_payload = if has_null {
        None
    } else {
        serde_json::from_value::<crate::ws::messages::EdgeToServerMsg>(payload).ok()
    };
    match parsed_payload {
        Some(crate::ws::messages::EdgeToServerMsg::SpotUpdate {
            spot_id,
            status,
            confidence,
            ..
        }) => {
            if status != "free" && status != "occupied" {
                delivery.ack.reason = Some("invalid_status".to_owned());
            } else if confidence < 0.20 {
                delivery.ack.outcome = Outcome::Ignored;
                delivery.ack.reason = Some("low_confidence".to_owned());
            } else {
                let current = sqlx::query_scalar::<_, String>(
                    "SELECT status FROM spots WHERE id = $1 FOR NO KEY UPDATE",
                )
                .bind(&spot_id)
                .fetch_optional(&mut *tx)
                .await?;
                match current {
                    None => delivery.ack.reason = Some("unknown_spot".to_owned()),
                    Some(current) if current == "reserved" && status == "free" => {
                        delivery.ack.outcome = Outcome::Ignored;
                        delivery.ack.reason = Some("reservation_protected".to_owned());
                    }
                    Some(_) => {
                        sqlx::query(
                            "UPDATE spots SET status = $2, last_updated = NOW() WHERE id = $1",
                        )
                        .bind(&spot_id)
                        .bind(&status)
                        .execute(&mut *tx)
                        .await?;
                        if status == "free" {
                            sqlx::query(
                                "UPDATE user_occupancy_history SET released_at = NOW() \
                                 WHERE spot_id = $1 AND released_at IS NULL",
                            )
                            .bind(&spot_id)
                            .execute(&mut *tx)
                            .await?;
                        } else {
                            delivery.unconfirmed_reservation = sqlx::query_scalar(
                                "SELECT id FROM reservations WHERE spot_id = $1 AND status = 'active'",
                            )
                            .bind(&spot_id)
                            .fetch_optional(&mut *tx)
                            .await?;
                        }
                        delivery.ack.outcome = Outcome::Applied;
                        delivery.ack.reason = None;
                        delivery.spot_update = Some((spot_id, status));
                    }
                }
            }
        }
        Some(_) => delivery.ack.reason = Some("unsupported_event".to_owned()),
        None => {}
    }
    sqlx::query("UPDATE edge_event_receipts SET outcome = $2, reason = $3 WHERE event_id = $1")
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
    use tokio::sync::Barrier;

    use super::*;
    use crate::reservation_store;

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
    async fn occupied_receipt_and_captured_reservation_are_committed(
        pool: PgPool,
    ) -> Result<(), sqlx::Error> {
        let spot_id = spot(&pool, "free").await?;
        let owner = user(&pool).await?;
        let reservation =
            reservation_store::create(&pool, &spot_id, owner, Utc::now() + Duration::minutes(15))
                .await
                .expect("fixture reservation must be created");
        let id = Uuid::new_v4();
        let delivery = apply(&pool, id, payload(&spot_id, "occupied")).await?;

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
    async fn duplicate_returns_original_ack_without_reapplying(
        pool: PgPool,
    ) -> Result<(), sqlx::Error> {
        let spot_id = spot(&pool, "free").await?;
        let id = Uuid::new_v4();
        let first_payload = payload(&spot_id, "occupied");
        let first = apply(&pool, id, first_payload.clone()).await?;
        assert_eq!(first.ack.outcome, Outcome::Applied);
        sqlx::query("UPDATE spots SET status = 'free' WHERE id = $1")
            .bind(&spot_id)
            .execute(&pool)
            .await?;

        // JSONB equality also considers 1 and 1.0 equal, independently of key order.
        let mut canonical_duplicate = first_payload;
        canonical_duplicate["confidence"] = json!(1);
        let duplicate = apply(&pool, id, canonical_duplicate).await?;
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
    async fn concurrent_duplicate_mutates_spot_once(pool: PgPool) -> Result<(), sqlx::Error> {
        let spot_id = spot(&pool, "free").await?;
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
            let payload = event_payload.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                apply(&pool, id, payload).await
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
    async fn reused_id_rejects_changed_payload_and_preserves_original(
        pool: PgPool,
    ) -> Result<(), sqlx::Error> {
        let spot_id = spot(&pool, "free").await?;
        let id = Uuid::new_v4();
        let original = payload(&spot_id, "occupied");
        assert_eq!(
            apply(&pool, id, original.clone()).await?.ack.outcome,
            Outcome::Applied
        );

        let conflict = apply(&pool, id, payload(&spot_id, "free")).await?;
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
        assert!(apply(&pool, id, original).await?.ack.duplicate);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn free_event_releases_history_and_commits_receipt(
        pool: PgPool,
    ) -> Result<(), sqlx::Error> {
        let spot_id = spot(&pool, "occupied").await?;
        let history_id = history(&pool, &spot_id).await?;
        let id = Uuid::new_v4();
        let delivery = apply(&pool, id, payload(&spot_id, "free")).await?;
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
    async fn finalization_error_rolls_back_receipt_spot_and_history(
        pool: PgPool,
    ) -> Result<(), sqlx::Error> {
        let spot_id = spot(&pool, "occupied").await?;
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
        assert!(apply(&pool, id, event_payload.clone()).await.is_err());
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
        let retry = apply(&pool, id, event_payload).await?;
        assert_eq!(retry.ack.outcome, Outcome::Applied);
        assert!(!retry.ack.duplicate);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ignored_events_preserve_spot_and_history_and_are_deduplicated(
        pool: PgPool,
    ) -> Result<(), sqlx::Error> {
        let spot_id = spot(&pool, "reserved").await?;
        let history_id = history(&pool, &spot_id).await?;
        let mut low_confidence = payload(&spot_id, "occupied");
        low_confidence["confidence"] = json!(0.19);
        let protected = payload(&spot_id, "free");
        for (event_payload, reason) in [
            (low_confidence, "low_confidence"),
            (protected, "reservation_protected"),
        ] {
            let id = Uuid::new_v4();
            let first = apply(&pool, id, event_payload.clone()).await?;
            assert_eq!(first.ack.outcome, Outcome::Ignored);
            assert_eq!(first.ack.reason.as_deref(), Some(reason));
            assert!(!first.ack.duplicate);
            assert_no_effects(&first);
            let duplicate = apply(&pool, id, event_payload).await?;
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
    async fn invalid_and_unsupported_payloads_have_terminal_receipts(
        pool: PgPool,
    ) -> Result<(), sqlx::Error> {
        let spot_id = spot(&pool, "free").await?;
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
            (payload("unknown-delivery-spot", "free"), "unknown_spot"),
            (
                json!({"type": "PATH_UPDATE", "from_node": "a", "to_node": "b", "is_active": true}),
                "unsupported_event",
            ),
        ];
        for (event_payload, reason) in cases {
            let id = Uuid::new_v4();
            let first = apply(&pool, id, event_payload.clone()).await?;
            assert_eq!(first.ack.outcome, Outcome::Rejected);
            assert_eq!(first.ack.reason.as_deref(), Some(reason));
            assert_no_effects(&first);
            assert_eq!(
                receipt(&pool, id).await?,
                ("rejected".to_owned(), Some(reason.to_owned()))
            );
            let duplicate = apply(&pool, id, event_payload).await?;
            assert_eq!(duplicate.ack.outcome, Outcome::Rejected);
            assert_eq!(duplicate.ack.reason, first.ack.reason);
            assert!(duplicate.ack.duplicate);
            assert_no_effects(&duplicate);
        }
        assert_eq!(status(&pool, &spot_id).await?, "free");
        Ok(())
    }
}

// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Data subject rights, personal history retention, the administrative trail
//! and readiness.
use axum::{
    Json,
    body::to_bytes,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::NaiveDate;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{personal_data::Field, reservation_tests::Fixture, users};

async fn json(response: Response) -> Value {
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn declare_eligibility(f: &Fixture, user: Uuid) {
    let cipher = &f.state.personal_data;
    let born = cipher
        .encrypt(
            Field::UserDateOfBirth,
            user,
            &Some(NaiveDate::from_ymd_opt(1950, 3, 4).unwrap()),
        )
        .unwrap();
    let pcd = cipher
        .encrypt(Field::UserPcdStatus, user, &Some(true))
        .unwrap();
    sqlx::query("UPDATE users SET date_of_birth_enc = $2, pcd_status_enc = $3 WHERE id = $1")
        .bind(user)
        .bind(born)
        .bind(pcd)
        .execute(f.state.installation.pool())
        .await
        .unwrap();
}

async fn visit(f: &Fixture, user: Uuid, days_ago: i32) {
    sqlx::query(
        "INSERT INTO user_occupancy_history (user_id, spot_id, occupied_at) \
         VALUES ($1, $2, NOW() - make_interval(days => $3))",
    )
    .bind(user)
    .bind(&f.spot_id)
    .bind(days_ago)
    .execute(f.state.installation.pool())
    .await
    .unwrap();
}

async fn count(pool: &PgPool, sql: &'static str, user: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn export_holds_exactly_the_requesters_data_decrypted(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    declare_eligibility(&f, f.owner).await;
    let reservation = f.create_for_owner().await;
    visit(&f, f.owner, 3).await;
    visit(&f, f.other, 3).await;

    let response =
        users::export_personal_data(State(f.state.clone()), f.headers(f.owner, "user").await)
            .await
            .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let export = json(response).await;
    assert_eq!(export["account"]["id"], f.owner.to_string());
    assert_eq!(export["account"]["date_of_birth"], "1950-03-04");
    assert_eq!(export["account"]["pcd_status"], true);
    let reservations = export["reservations"].as_array().unwrap();
    assert_eq!(reservations.len(), 1);
    assert_eq!(reservations[0]["id"], reservation.to_string());
    assert_eq!(export["occupancy_history"].as_array().unwrap().len(), 1);
    assert!(!export["sessions"].as_array().unwrap().is_empty());
    let text = export.to_string();
    assert!(
        !text.contains(&f.other.to_string()),
        "no other person's data"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn deleting_an_account_releases_its_spot_and_removes_its_data(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    f.create_for_owner().await;
    visit(&f, f.owner, 3).await;
    visit(&f, f.other, 3).await;
    let owner_headers = f.headers(f.owner, "user").await;
    let mut updates = f.state.installation.tx.subscribe();

    let response = users::delete_account(State(f.state.clone()), owner_headers.clone())
        .await
        .into_response();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cleared = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");

    let pool = f.state.installation.pool();
    let status: String = sqlx::query_scalar("SELECT status FROM spots WHERE id = $1")
        .bind(&f.spot_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(status, "free");
    let update: Value = serde_json::from_str(&updates.recv().await.unwrap()).unwrap();
    assert_eq!(update["status"], "free");
    for sql in [
        "SELECT COUNT(*) FROM users WHERE id = $1",
        "SELECT COUNT(*) FROM reservations WHERE user_id = $1",
        "SELECT COUNT(*) FROM user_occupancy_history WHERE user_id = $1",
        "SELECT COUNT(*) FROM auth_sessions WHERE user_id = $1",
    ] {
        assert_eq!(count(pool, sql, f.owner).await, 0, "{sql}");
    }
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM user_occupancy_history WHERE user_id = $1",
            f.other
        )
        .await,
        1,
        "other people's data stays"
    );
    // The old session is gone with the account.
    let me = users::me(State(f.state.clone()), owner_headers).await;
    assert_eq!(
        me.err().map(|(status, _)| status),
        Some(StatusCode::UNAUTHORIZED)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn personal_history_expires_after_its_retention(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    visit(&f, f.owner, crate::sessions::PERSONAL_HISTORY_DAYS + 1).await;
    visit(&f, f.owner, crate::sessions::PERSONAL_HISTORY_DAYS - 1).await;
    crate::sessions::cleanup_expired(f.state.installation.pool())
        .await
        .unwrap();
    let kept: Vec<i32> = sqlx::query_scalar(
        "SELECT EXTRACT(DAY FROM NOW() - occupied_at)::int FROM user_occupancy_history",
    )
    .fetch_all(f.state.installation.pool())
    .await
    .unwrap();
    assert_eq!(kept, [crate::sessions::PERSONAL_HISTORY_DAYS - 1]);
}

async fn trail(pool: &PgPool) -> Vec<(Uuid, String, Option<String>)> {
    sqlx::query_as("SELECT actor_user_id, action, target FROM admin_audit_log ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn administrative_actions_leave_a_trail_and_self_service_does_not(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    let admin = f.headers(f.admin, "admin").await;

    // Reading one's own account is not an administrative action.
    users::get_user(
        State(f.state.clone()),
        Path(f.owner),
        f.headers(f.owner, "user").await,
    )
    .await
    .unwrap();
    assert!(trail(f.state.installation.pool()).await.is_empty());

    users::get_user(State(f.state.clone()), Path(f.owner), admin.clone())
        .await
        .unwrap();
    let response = crate::reservations::update_spot_status(
        State(f.state.clone()),
        Path(f.spot_id.clone()),
        admin.clone(),
        Json(crate::reservations::UpdateSpotStatus {
            status: "occupied".into(),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    crate::reservations::get_reservations(State(f.state.clone()), admin.clone())
        .await
        .unwrap();
    // A failed action records nothing: the trail shares its transaction.
    let missing = crate::reservations::update_spot_status(
        State(f.state.clone()),
        Path("missing-spot".into()),
        admin,
        Json(crate::reservations::UpdateSpotStatus {
            status: "free".into(),
        }),
    )
    .await
    .into_response();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        trail(f.state.installation.pool()).await,
        [
            (f.admin, "user_read".to_owned(), Some(f.owner.to_string())),
            (
                f.admin,
                "spot_status_set".to_owned(),
                Some(f.spot_id.clone())
            ),
            (f.admin, "active_reservations_listed".to_owned(), None),
        ]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn readiness_needs_the_database_bound_to_this_installation(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    assert_eq!(
        crate::readiness(State(f.state.clone())).await.0,
        StatusCode::OK
    );
    sqlx::query("UPDATE installation_binding SET installation_id = gen_random_uuid()")
        .execute(f.state.installation.pool())
        .await
        .unwrap();
    assert_eq!(
        crate::readiness(State(f.state.clone())).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    f.state.installation.pool().close().await;
    assert_eq!(
        crate::readiness(State(f.state.clone())).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

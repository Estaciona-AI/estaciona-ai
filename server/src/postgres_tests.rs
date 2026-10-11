use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn migrations_persist_a_reservation_with_user_and_spot(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let user_id = Uuid::new_v4();
    let reservation_id = Uuid::new_v4();
    let spot_id = "migration-test-spot";

    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash) \
         VALUES ($1, 'Migration Fixture', 'migration@example.invalid', 'unused-test-hash')",
    )
    .bind(user_id)
    .execute(&pool)
    .await?;

    sqlx::query("INSERT INTO spots (id, status, x, y) VALUES ($1, 'free', 0, 0)")
        .bind(spot_id)
        .execute(&pool)
        .await?;

    sqlx::query(
        "INSERT INTO reservations (id, user_id, spot_id, status, expires_at) \
         VALUES ($1, $2, $3, 'active', NOW() + INTERVAL '15 minutes')",
    )
    .bind(reservation_id)
    .bind(user_id)
    .bind(spot_id)
    .execute(&pool)
    .await?;

    let persisted: (Uuid, String, String) = sqlx::query_as(
        "SELECT users.id, spots.id, reservations.status FROM reservations \
         JOIN users ON users.id = reservations.user_id \
         JOIN spots ON spots.id = reservations.spot_id \
         WHERE reservations.id = $1",
    )
    .bind(reservation_id)
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        persisted,
        (user_id, spot_id.to_owned(), "active".to_owned())
    );

    Ok(())
}

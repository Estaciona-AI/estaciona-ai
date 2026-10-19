// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn migration_leaves_database_unbound(pool: PgPool) -> Result<(), sqlx::Error> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM public.installation_binding")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn binding_schema_enforces_singleton_and_valid_coordinates(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let organization = Uuid::from_u128(1);
    let installation = Uuid::from_u128(2);
    for (org, site, environment, singleton) in [
        (Uuid::nil(), installation, "test", true),
        (organization, Uuid::nil(), "test", true),
        (organization, installation, "staging", true),
        (organization, installation, "test", false),
    ] {
        let error=sqlx::query("INSERT INTO public.installation_binding (singleton,organization_id,installation_id,environment) VALUES ($1,$2,$3,$4)")
            .bind(singleton).bind(org).bind(site).bind(environment).execute(&pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }
    sqlx::query("INSERT INTO public.installation_binding (organization_id,installation_id,environment) VALUES ($1,$2,'test')")
        .bind(organization).bind(installation).execute(&pool).await?;
    let error=sqlx::query("INSERT INTO public.installation_binding (organization_id,installation_id,environment) VALUES ($1,$2,'test')")
        .bind(Uuid::from_u128(3)).bind(Uuid::from_u128(4)).execute(&pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23505")
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn downgrade_refuses_a_bound_database(pool: PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO public.installation_binding (organization_id,installation_id,environment) VALUES ($1,$2,'test')")
        .bind(Uuid::from_u128(1)).bind(Uuid::from_u128(2)).execute(&pool).await?;
    let mut tx = pool.begin().await?;
    assert!(
        sqlx::raw_sql(include_str!(
            "../../migrations/20261019010000_installation_binding.down.sql"
        ))
        .execute(&mut *tx)
        .await
        .is_err()
    );
    tx.rollback().await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM public.installation_binding")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 1);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn downgrade_can_remove_an_unbound_schema(pool: PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/20261019010000_installation_binding.down.sql"
    ))
    .execute(&mut *tx)
    .await?;
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.installation_binding') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
    assert!(!exists);
    tx.rollback().await?;
    Ok(())
}

use std::{fmt, time::Duration};

use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub(crate) fn credential_digest(credential: &str) -> [u8; 32] {
    Sha256::digest(credential.as_bytes()).into()
}

#[derive(Clone, Debug)]
pub struct Principal {
    pub gateway_id: Uuid,
    digest: [u8; 32],
}

#[derive(Debug)]
pub enum AuthError {
    Unauthenticated,
    Database(sqlx::Error),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unauthenticated => "Invalid edge gateway credential",
            Self::Database(_) => "Edge authorization unavailable",
        })
    }
}

impl std::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Unauthenticated => None,
        }
    }
}

impl From<sqlx::Error> for AuthError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

pub async fn authenticate(
    pool: &PgPool,
    credential: &str,
) -> Result<Option<Principal>, sqlx::Error> {
    if credential.len() != 64 || !credential.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let digest = credential_digest(credential);
    let gateway_id = tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM edge_gateways WHERE credential_digest = $1 AND revoked_at IS NULL",
        )
        .bind(digest.as_slice())
        .fetch_optional(pool),
    )
    .await
    .map_err(|_| sqlx::Error::PoolTimedOut)??;
    Ok(gateway_id.map(|gateway_id| Principal { gateway_id, digest }))
}

pub async fn begin_authorized<'p>(
    pool: &'p PgPool,
    principal: &Principal,
) -> Result<Transaction<'p, Postgres>, AuthError> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut tx = pool.begin().await?;
        sqlx::raw_sql("SET LOCAL statement_timeout = '2s'; SET LOCAL lock_timeout = '2s'")
            .execute(&mut *tx)
            .await?;
        let authorized = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM edge_gateways \
             WHERE id = $1 AND credential_digest = $2 AND revoked_at IS NULL FOR SHARE",
        )
        .bind(principal.gateway_id)
        .bind(principal.digest.as_slice())
        .fetch_optional(&mut *tx)
        .await?;
        if authorized.is_none() {
            return Err(AuthError::Unauthenticated);
        }
        Ok(tx)
    })
    .await
    .map_err(|_| AuthError::Database(sqlx::Error::PoolTimedOut))?
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    async fn fixture(pool: &PgPool) -> Result<Principal, sqlx::Error> {
        let gateway_id = Uuid::new_v4();
        let digest = credential_digest(TOKEN);
        sqlx::query(
            "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Fixture', $2)",
        )
        .bind(gateway_id)
        .bind(digest.as_slice())
        .execute(pool)
        .await?;
        Ok(Principal { gateway_id, digest })
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn authenticates_only_registered_active_credentials(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let principal = fixture(&pool).await?;
        let authenticated = authenticate(&pool, TOKEN)
            .await?
            .expect("registered credential must authenticate");
        assert_eq!(authenticated.gateway_id, principal.gateway_id);
        assert!(authenticate(&pool, &"f".repeat(64)).await?.is_none());
        sqlx::query("UPDATE edge_gateways SET revoked_at = NOW() WHERE id = $1")
            .bind(principal.gateway_id)
            .execute(&pool)
            .await?;
        assert!(authenticate(&pool, TOKEN).await?.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn malformed_credentials_bypass_database_and_valid_lookups_fail_closed(
        pool: PgPool,
    ) -> Result<(), sqlx::Error> {
        pool.close().await;
        for token in ["", "not-a-credential", "invalid\0", &"z".repeat(64)] {
            assert!(authenticate(&pool, token).await?.is_none());
        }
        assert!(matches!(
            authenticate(&pool, TOKEN).await,
            Err(sqlx::Error::PoolClosed)
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn rotated_or_revoked_principals_cannot_begin_mutations(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let principal = fixture(&pool).await?;
        let tx = begin_authorized(&pool, &principal).await?;
        tx.commit().await?;
        let replacement = "e".repeat(64);
        sqlx::query("UPDATE edge_gateways SET credential_digest = $2 WHERE id = $1")
            .bind(principal.gateway_id)
            .bind(credential_digest(&replacement).as_slice())
            .execute(&pool)
            .await?;
        assert!(matches!(
            begin_authorized(&pool, &principal).await,
            Err(AuthError::Unauthenticated)
        ));
        assert!(authenticate(&pool, TOKEN).await?.is_none());
        let rotated = authenticate(&pool, &replacement)
            .await?
            .expect("rotated credential must authenticate");
        assert_eq!(rotated.gateway_id, principal.gateway_id);
        sqlx::query("UPDATE edge_gateways SET revoked_at = NOW() WHERE id = $1")
            .bind(principal.gateway_id)
            .execute(&pool)
            .await?;
        assert!(matches!(
            begin_authorized(&pool, &rotated).await,
            Err(AuthError::Unauthenticated)
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn authorization_bounds_pool_acquisition(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let principal = fixture(&pool).await?;
        let mut held = Vec::new();
        for _ in 0..pool.options().get_max_connections() {
            held.push(pool.acquire().await?);
        }
        let (lookup, begin) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(3), authenticate(&pool, TOKEN)),
            tokio::time::timeout(Duration::from_secs(3), begin_authorized(&pool, &principal)),
        );
        drop(held);
        assert!(matches!(
            lookup.expect("credential lookup must have its own deadline"),
            Err(sqlx::Error::PoolTimedOut)
        ));
        assert!(matches!(
            begin.expect("transaction acquisition must have its own deadline"),
            Err(AuthError::Database(sqlx::Error::PoolTimedOut))
        ));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn authorized_transactions_bound_database_statement_work(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let principal = fixture(&pool).await?;
        let mut tx = begin_authorized(&pool, &principal).await?;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            sqlx::query("SELECT pg_sleep(4)").execute(&mut *tx),
        )
        .await;
        let error = result
            .expect("statement timeout must be enforced inside the transaction")
            .unwrap_err();
        assert_eq!(
            error
                .as_database_error()
                .and_then(|error| error.code())
                .as_deref(),
            Some("57014")
        );
        tx.rollback().await?;
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn authority_lock_serializes_mutations_with_cli_revocation(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let principal = fixture(&pool).await?;
        let tx = begin_authorized(&pool, &principal).await?;
        let admin_pool = pool.clone();
        let id = principal.gateway_id;
        let revoke = tokio::spawn(async move {
            crate::edge_cli::dispatch(
                &admin_pool,
                &[
                    "server".to_owned(),
                    "--edge-revoke".to_owned(),
                    id.to_string(),
                ],
            )
            .await
        });
        let waited = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE '%SELECT revoked_at FROM edge_gateways%FOR UPDATE%')").fetch_one(&pool).await?;
                if blocked { return Ok::<(), sqlx::Error>(()); }
                if revoke.is_finished() { panic!("revocation must wait for the authorized transaction"); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        tx.commit().await?;
        assert!(revoke.await??);
        waited.expect("CLI revocation must block on the authority row")?;
        assert!(matches!(
            begin_authorized(&pool, &principal).await,
            Err(AuthError::Unauthenticated)
        ));
        Ok(())
    }
}

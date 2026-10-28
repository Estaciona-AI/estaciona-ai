// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Debug)]
pub enum CliError {
    Usage,
    InvalidPolicy,
    UnknownGateway,
    RevokedGateway,
    Io(io::Error),
    Database(sqlx::Error),
    Random,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Usage => "Invalid edge administration command",
            Self::InvalidPolicy => "Invalid edge gateway policy",
            Self::UnknownGateway => "Unknown edge gateway",
            Self::RevokedGateway => "Edge gateway has been revoked",
            Self::Io(_) => "Edge credential or policy file operation failed",
            Self::Database(_) => "Edge administration database operation failed",
            Self::Random => "Edge credential generation failed",
        })
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<sqlx::Error> for CliError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    path_update: bool,
    trend_prediction: bool,
    cameras: Vec<CameraPolicy>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CameraPolicy {
    camera_id: String,
    spot_update: bool,
    car_detected: bool,
    spots: Vec<String>,
}

impl Policy {
    fn load(path: &Path) -> Result<Self, CliError> {
        let policy: Self =
            serde_json::from_reader(File::open(path)?).map_err(|_| CliError::InvalidPolicy)?;
        let mut cameras = HashSet::new();
        for camera in &policy.cameras {
            if camera.camera_id.trim().is_empty()
                || camera.camera_id.contains('\0')
                || camera.camera_id.chars().count() > 50
                || !cameras.insert(&camera.camera_id)
            {
                return Err(CliError::InvalidPolicy);
            }
            let mut spots = HashSet::new();
            for spot in &camera.spots {
                if spot.trim().is_empty() || spot.contains('\0') || !spots.insert(spot) {
                    return Err(CliError::InvalidPolicy);
                }
            }
        }
        Ok(policy)
    }
}

struct PendingSecretFile {
    path: PathBuf,
    file: File,
    committed: bool,
}

impl PendingSecretFile {
    fn create(path: &Path, credential: &str) -> Result<Self, io::Error> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let mut pending = Self {
            path: path.to_owned(),
            file,
            committed: false,
        };
        pending.file.write_all(credential.as_bytes())?;
        pending.file.write_all(b"\n")?;
        pending.file.sync_all()?;
        Ok(pending)
    }
}

impl Drop for PendingSecretFile {
    fn drop(&mut self) {
        if !self.committed
            && let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "Failed to remove incomplete edge credential file");
        }
    }
}

fn new_credential() -> Result<String, CliError> {
    let mut bytes = [0_u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| CliError::Random)?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut credential = String::with_capacity(64);
    for byte in bytes {
        credential.push(HEX[usize::from(byte >> 4)] as char);
        credential.push(HEX[usize::from(byte & 15)] as char);
    }
    Ok(credential)
}

async fn begin_mutation(pool: &PgPool) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql("SET LOCAL statement_timeout = '2s'; SET LOCAL lock_timeout = '2s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn lock_gateway(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, CliError> {
    sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "SELECT revoked_at FROM edge_gateways WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(CliError::UnknownGateway)
}

async fn create(pool: &PgPool, name: &str, path: &Path) -> Result<(), CliError> {
    let credential = new_credential()?;
    let id = Uuid::new_v4();
    let mut tx = begin_mutation(pool).await?;
    sqlx::query("INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(name)
        .bind(crate::edge_auth::credential_digest(&credential).as_slice())
        .execute(&mut *tx)
        .await?;
    let mut pending = PendingSecretFile::create(path, &credential)?;
    tx.commit().await?;
    pending.committed = true;
    tracing::info!(gateway_id = %id, "Edge gateway created");
    Ok(())
}

async fn rotate(pool: &PgPool, id: Uuid, path: &Path) -> Result<(), CliError> {
    let mut tx = begin_mutation(pool).await?;
    if lock_gateway(&mut tx, id).await?.is_some() {
        return Err(CliError::RevokedGateway);
    }
    let credential = new_credential()?;
    sqlx::query("UPDATE edge_gateways SET credential_digest = $2 WHERE id = $1")
        .bind(id)
        .bind(crate::edge_auth::credential_digest(&credential).as_slice())
        .execute(&mut *tx)
        .await?;
    let mut pending = PendingSecretFile::create(path, &credential)?;
    tx.commit().await?;
    pending.committed = true;
    tracing::info!(gateway_id = %id, "Edge gateway credential rotated");
    Ok(())
}

async fn revoke(pool: &PgPool, id: Uuid) -> Result<(), CliError> {
    let mut tx = begin_mutation(pool).await?;
    lock_gateway(&mut tx, id).await?;
    sqlx::query("UPDATE edge_gateways SET revoked_at = COALESCE(revoked_at, NOW()) WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(gateway_id = %id, "Edge gateway revoked");
    Ok(())
}

async fn replace_policy(pool: &PgPool, id: Uuid, policy: Policy) -> Result<(), CliError> {
    let mut tx = begin_mutation(pool).await?;
    lock_gateway(&mut tx, id).await?;
    sqlx::query("UPDATE edge_gateways SET path_update = $2, trend_prediction = $3 WHERE id = $1")
        .bind(id)
        .bind(policy.path_update)
        .bind(policy.trend_prediction)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM edge_camera_grants WHERE gateway_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for camera in policy.cameras {
        sqlx::query("INSERT INTO edge_camera_grants (gateway_id, camera_id, spot_update, car_detected) VALUES ($1, $2, $3, $4)")
            .bind(id).bind(&camera.camera_id).bind(camera.spot_update).bind(camera.car_detected)
            .execute(&mut *tx).await?;
        for spot in camera.spots {
            sqlx::query("INSERT INTO edge_camera_spots (gateway_id, camera_id, spot_id) VALUES ($1, $2, $3)")
                .bind(id).bind(&camera.camera_id).bind(spot).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    tracing::info!(gateway_id = %id, "Edge gateway policy replaced");
    Ok(())
}

pub async fn dispatch(pool: &PgPool, args: &[String]) -> Result<bool, CliError> {
    let Some(command) = args.get(1).map(String::as_str) else {
        return Ok(false);
    };
    match command {
        "--edge-create" if args.len() == 5 && args[3] == "--secret-file" => {
            if args[2].trim().is_empty() || args[2].contains('\0') {
                return Err(CliError::Usage);
            }
            create(pool, &args[2], Path::new(&args[4])).await?;
        }
        "--edge-rotate" if args.len() == 5 && args[3] == "--secret-file" => {
            let id = Uuid::parse_str(&args[2]).map_err(|_| CliError::Usage)?;
            rotate(pool, id, Path::new(&args[4])).await?;
        }
        "--edge-revoke" if args.len() == 3 => {
            let id = Uuid::parse_str(&args[2]).map_err(|_| CliError::Usage)?;
            revoke(pool, id).await?;
        }
        "--edge-policy" if args.len() == 4 => {
            let id = Uuid::parse_str(&args[2]).map_err(|_| CliError::Usage)?;
            let policy = Policy::load(Path::new(&args[3]))?;
            replace_policy(pool, id, policy).await?;
        }
        command if command.starts_with("--edge-") => return Err(CliError::Usage),
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    use uuid::Uuid;

    struct Files(PathBuf);

    impl Files {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("r05-edge-cli-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        std::iter::once("server")
            .chain(values.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    async fn fixture(pool: &PgPool, seed: u8) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Fixture', $2)",
        )
        .bind(id)
        .bind(vec![seed; 32])
        .execute(pool)
        .await?;
        Ok(id)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn creates_private_unique_credentials_without_implicit_grants(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let files = Files::new();
        let mut tokens = Vec::new();
        for name in ["first", "second"] {
            let path = files.path(name);
            assert!(
                dispatch(
                    &pool,
                    &args(&[
                        "--edge-create",
                        name,
                        "--secret-file",
                        path.to_str().unwrap()
                    ])
                )
                .await?
            );
            let text = fs::read_to_string(&path)?;
            let token = text.trim_end();
            assert_eq!(token.len(), 64);
            assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
            let row: (Vec<u8>, bool, bool) = sqlx::query_as("SELECT credential_digest, path_update, trend_prediction FROM edge_gateways WHERE name = $1")
                .bind(name).fetch_one(&pool).await?;
            assert_eq!(
                row,
                (Sha256::digest(token.as_bytes()).to_vec(), false, false)
            );
            tokens.push(token.to_owned());
        }
        assert_ne!(tokens[0], tokens[1]);
        let grants: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM edge_camera_grants) + (SELECT COUNT(*) FROM edge_camera_spots)").fetch_one(&pool).await?;
        assert_eq!(grants, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refuses_existing_files_and_symlinks_without_creating_gateway(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let files = Files::new();
        let existing = files.path("existing");
        fs::write(&existing, "preserve")?;
        let link = files.path("symlink");
        std::os::unix::fs::symlink(&existing, &link)?;
        for path in [&existing, &link] {
            assert!(matches!(
                dispatch(
                    &pool,
                    &args(&[
                        "--edge-create",
                        "Fixture",
                        "--secret-file",
                        path.to_str().unwrap()
                    ])
                )
                .await,
                Err(CliError::Io(_))
            ));
        }
        assert_eq!(fs::read_to_string(existing)?, "preserve");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edge_gateways")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn rotation_preserves_gateway_and_grants_and_revocation_is_final(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let files = Files::new();
        let id = fixture(&pool, 1).await?;
        sqlx::query("INSERT INTO edge_camera_grants (gateway_id, camera_id, spot_update, car_detected) VALUES ($1, 'cam', true, false)").bind(id).execute(&pool).await?;
        let secret = files.path("rotated");
        assert!(
            dispatch(
                &pool,
                &args(&[
                    "--edge-rotate",
                    &id.to_string(),
                    "--secret-file",
                    secret.to_str().unwrap()
                ])
            )
            .await?
        );
        let token = fs::read_to_string(&secret)?;
        let row: (Vec<u8>, i64) = sqlx::query_as("SELECT credential_digest, (SELECT COUNT(*) FROM edge_camera_grants WHERE gateway_id = $1) FROM edge_gateways WHERE id = $1").bind(id).fetch_one(&pool).await?;
        assert_eq!(
            row,
            (Sha256::digest(token.trim_end().as_bytes()).to_vec(), 1)
        );
        for _ in 0..2 {
            assert!(dispatch(&pool, &args(&["--edge-revoke", &id.to_string()])).await?);
        }
        let another_secret = files.path("denied");
        assert!(matches!(
            dispatch(
                &pool,
                &args(&[
                    "--edge-rotate",
                    &id.to_string(),
                    "--secret-file",
                    another_secret.to_str().unwrap()
                ])
            )
            .await,
            Err(CliError::RevokedGateway)
        ));
        assert!(!another_secret.exists());
        let revoked: bool =
            sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM edge_gateways WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert!(revoked);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn policy_replaces_only_selected_gateway_authority(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let files = Files::new();
        let gateway = fixture(&pool, 2).await?;
        let other = fixture(&pool, 3).await?;
        sqlx::query("INSERT INTO spots (id,status,x,y) VALUES ('A','free',0,0),('B','free',1,0)")
            .execute(&pool)
            .await?;
        for id in [gateway, other] {
            sqlx::query("INSERT INTO edge_camera_grants (gateway_id,camera_id,spot_update,car_detected) VALUES ($1,'old',true,true)").bind(id).execute(&pool).await?;
            sqlx::query("INSERT INTO edge_camera_spots (gateway_id,camera_id,spot_id) VALUES ($1,'old','A')").bind(id).execute(&pool).await?;
        }
        let policy = files.path("policy.json");
        fs::write(
            &policy,
            r#"{"path_update":true,"trend_prediction":false,"cameras":[{"camera_id":"new","spot_update":false,"car_detected":true,"spots":["B"]}]}"#,
        )?;
        assert!(
            dispatch(
                &pool,
                &args(&[
                    "--edge-policy",
                    &gateway.to_string(),
                    policy.to_str().unwrap()
                ])
            )
            .await?
        );
        let rows: Vec<(Uuid, String, String, bool, bool)> = sqlx::query_as("SELECT g.gateway_id,g.camera_id,s.spot_id,g.spot_update,g.car_detected FROM edge_camera_grants g JOIN edge_camera_spots s USING (gateway_id,camera_id) ORDER BY camera_id").fetch_all(&pool).await?;
        assert_eq!(
            rows,
            [
                (gateway, "new".to_owned(), "B".to_owned(), false, true),
                (other, "old".to_owned(), "A".to_owned(), true, true)
            ]
        );
        let global: (bool, bool) =
            sqlx::query_as("SELECT path_update,trend_prediction FROM edge_gateways WHERE id = $1")
                .bind(gateway)
                .fetch_one(&pool)
                .await?;
        assert_eq!(global, (true, false));
        fs::write(
            &policy,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[]}"#,
        )?;
        dispatch(
            &pool,
            &args(&[
                "--edge-policy",
                &gateway.to_string(),
                policy.to_str().unwrap(),
            ]),
        )
        .await?;
        let counts: (i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM edge_camera_grants WHERE gateway_id = $1),(SELECT COUNT(*) FROM edge_camera_spots WHERE gateway_id = $1)").bind(gateway).fetch_one(&pool).await?;
        assert_eq!(counts, (0, 0));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn malformed_or_ambiguous_policy_is_rejected_before_database(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        pool.close().await;
        let files = Files::new();
        let policy = files.path("policy.json");
        for text in [
            r#"{"path_update":false,"trend_prediction":false,"cameras":[],"unexpected":true}"#,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[{"camera_id":"cam","spot_update":true,"car_detected":false,"spots":[],"unexpected":true}]}"#,
            r#"{"path_update":"true","trend_prediction":false,"cameras":[]}"#,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[{"camera_id":"cam","spot_update":true,"car_detected":false,"spots":["A","A"]}]}"#,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[{"camera_id":"cam","spot_update":true,"car_detected":false,"spots":[]},{"camera_id":"cam","spot_update":false,"car_detected":false,"spots":[]}]}"#,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[{"camera_id":"","spot_update":true,"car_detected":false,"spots":[]}]}"#,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[{"camera_id":"c\u0000","spot_update":true,"car_detected":false,"spots":[]}]}"#,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[{"camera_id":"cam","spot_update":true,"car_detected":false,"spots":[" "]}]}"#,
            r#"{"path_update":false,"trend_prediction":false,"cameras":[{"camera_id":"cam","spot_update":true,"car_detected":false,"spots":["A\u0000"]}]}"#,
            &format!(
                r#"{{"path_update":false,"trend_prediction":false,"cameras":[{{"camera_id":"{}","spot_update":true,"car_detected":false,"spots":[]}}]}}"#,
                "c".repeat(51)
            ),
        ] {
            fs::write(&policy, text)?;
            assert!(matches!(
                dispatch(
                    &pool,
                    &args(&[
                        "--edge-policy",
                        &Uuid::new_v4().to_string(),
                        policy.to_str().unwrap()
                    ])
                )
                .await,
                Err(CliError::InvalidPolicy)
            ));
        }
        // Fifty characters is the limit, counted as characters rather than bytes.
        fs::write(
            &policy,
            format!(
                r#"{{"path_update":false,"trend_prediction":false,"cameras":[{{"camera_id":"{}","spot_update":true,"car_detected":false,"spots":[]}}]}}"#,
                "é".repeat(50)
            ),
        )?;
        assert!(matches!(
            dispatch(
                &pool,
                &args(&[
                    "--edge-policy",
                    &Uuid::new_v4().to_string(),
                    policy.to_str().unwrap()
                ])
            )
            .await,
            Err(CliError::Database(_))
        ));
        assert!(!dispatch(&pool, &args(&["--create-admin", "fixture", "unused"])).await?);
        let secret = files.path("unexpected");
        let gateway = Uuid::new_v4().to_string();
        let secret_path = secret.to_str().unwrap();
        for command in [
            &["--edge-revoke", "not-a-uuid"][..],
            &["--edge-create", "Fixture", "--other-flag", secret_path],
            &["--edge-create", "Fixture", "--secret-file"],
            &["--edge-rotate", &gateway, "--other-flag", secret_path],
            &["--edge-rotate", &gateway, "--secret-file"],
            &["--edge-revoke"],
            &["--edge-policy", &gateway],
            &["--edge-unknown"],
        ] {
            assert!(
                matches!(dispatch(&pool, &args(command)).await, Err(CliError::Usage)),
                "{command:?}"
            );
        }
        assert!(!secret.exists());
        assert!(matches!(
            dispatch(
                &pool,
                &args(&[
                    "--edge-create",
                    " ",
                    "--secret-file",
                    files.path("empty").to_str().unwrap()
                ])
            )
            .await,
            Err(CliError::Usage)
        ));
        Ok(())
    }

    #[test]
    fn errors_name_the_operation_and_keep_their_cause() {
        let io = CliError::Io(io::Error::other("fixture"));
        assert_eq!(
            io.to_string(),
            "Edge credential or policy file operation failed"
        );
        assert!(std::error::Error::source(&io).is_some());
        let database = CliError::Database(sqlx::Error::PoolClosed);
        assert!(std::error::Error::source(&database).is_some());
        assert_eq!(
            CliError::Usage.to_string(),
            "Invalid edge administration command"
        );
        assert!(std::error::Error::source(&CliError::Usage).is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn failed_policy_preserves_previous_authority(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let files = Files::new();
        let id = fixture(&pool, 4).await?;
        sqlx::query("INSERT INTO edge_camera_grants (gateway_id,camera_id,spot_update,car_detected) VALUES ($1,'old',true,false)").bind(id).execute(&pool).await?;
        let policy = files.path("policy.json");
        fs::write(
            &policy,
            r#"{"path_update":true,"trend_prediction":true,"cameras":[{"camera_id":"new","spot_update":true,"car_detected":true,"spots":["missing"]}]}"#,
        )?;
        assert!(matches!(
            dispatch(
                &pool,
                &args(&["--edge-policy", &id.to_string(), policy.to_str().unwrap()])
            )
            .await,
            Err(CliError::Database(_))
        ));
        let authority: (bool, bool, String) = sqlx::query_as("SELECT path_update,trend_prediction,camera_id FROM edge_gateways JOIN edge_camera_grants ON edge_gateways.id = gateway_id WHERE id = $1").bind(id).fetch_one(&pool).await?;
        assert_eq!(authority, (false, false, "old".to_owned()));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn failed_commit_removes_new_secret_file(
        pool: PgPool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let files = Files::new();
        let id = fixture(&pool, 5).await?;
        sqlx::raw_sql("CREATE FUNCTION reject_gateway_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture rejected commit'; END $$; CREATE CONSTRAINT TRIGGER reject_gateway_commit AFTER INSERT OR UPDATE ON edge_gateways DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_gateway_commit()")
            .execute(&pool).await?;
        for (command, value, filename) in [
            ("--edge-create", "New".to_owned(), "create"),
            ("--edge-rotate", id.to_string(), "rotate"),
        ] {
            let path = files.path(filename);
            assert!(matches!(
                dispatch(
                    &pool,
                    &args(&[command, &value, "--secret-file", path.to_str().unwrap()])
                )
                .await,
                Err(CliError::Database(_))
            ));
            assert!(!path.exists());
        }
        let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT credential_digest FROM edge_gateways")
            .fetch_all(&pool)
            .await?;
        assert_eq!(rows, [vec![5_u8; 32]]);
        Ok(())
    }
}

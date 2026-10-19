// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::{io, path::PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub email: String,
    pub password_file: PathBuf,
}

pub(crate) fn parse(args: &[String]) -> Result<Option<Request>, &'static str> {
    if !args.get(1).is_some_and(|arg| arg == "--create-admin") {
        return Ok(None);
    }
    if args.len() != 5
        || args[3] != "--password-file"
        || args[2].is_empty()
        || args[2].chars().count() > 255
        || args[4].is_empty()
    {
        return Err("Usage: --create-admin EMAIL --password-file PATH");
    }
    Ok(Some(Request {
        email: args[2].clone(),
        password_file: PathBuf::from(&args[4]),
    }))
}

fn read_password(path: &std::path::Path) -> io::Result<String> {
    use std::{fs, io::Read, os::unix::fs::MetadataExt};
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Password requires a private regular file and one nonempty line of at most 1024 bytes",
        )
    };
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.mode() & 0o077 != 0 {
        return Err(invalid());
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || opened.mode() & 0o077 != 0
        || before.dev() != opened.dev()
        || before.ino() != opened.ino()
    {
        return Err(invalid());
    }
    let mut password = String::new();
    file.take(1027).read_to_string(&mut password)?;
    if password.ends_with('\n') {
        password.pop();
        if password.ends_with('\r') {
            password.pop();
        }
    }
    if password.is_empty() || password.len() > 1024 || password.contains(['\n', '\r']) {
        return Err(invalid());
    }
    Ok(password)
}

pub(crate) async fn create(
    pool: &sqlx::PgPool,
    request: Request,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hashed = tokio::task::spawn_blocking(move || {
        let password = read_password(&request.password_file)?;
        crate::security::hash_password(&password)
            .map_err(|_| io::Error::other("Password hashing failed"))
    })
    .await??;
    sqlx::query(
        "INSERT INTO users (id,name,email,password_hash,role) VALUES ($1,'Admin',$2,$3,'admin')",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(request.email)
    .bind(hashed)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::OpenOptionsExt,
    };

    #[sqlx::test(migrations = "./migrations")]
    async fn creates_an_admin_from_a_private_file(pool: sqlx::PgPool) {
        let path =
            std::env::temp_dir().join(format!("estaciona-admin-create-{}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"synthetic-test-password\n").unwrap();
        create(
            &pool,
            Request {
                email: "admin-file@example.invalid".into(),
                password_file: path.clone(),
            },
        )
        .await
        .unwrap();
        let (role, hash): (String, String) = sqlx::query_as(
            "SELECT role,password_hash FROM users WHERE email='admin-file@example.invalid'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "admin");
        assert!(crate::security::verify_password(
            "synthetic-test-password",
            &hash
        ));
        let link = path.with_extension("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_password(&link).is_err());
        fs::remove_file(link).unwrap();
        fs::write(&path, vec![b'a'; 1024]).unwrap();
        assert_eq!(read_password(&path).unwrap().len(), 1024);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn admin_cli_rejects_passwords_in_arguments() {
        let args: Vec<String> = [
            "server",
            "--create-admin",
            "admin@example.invalid",
            "synthetic-secret",
        ]
        .map(str::to_owned)
        .to_vec();
        let error = parse(&args).unwrap_err();
        assert!(!error.contains("synthetic-secret"));
        let args: Vec<String> = [
            "server",
            "--create-admin",
            "admin@example.invalid",
            "--password-file",
            "/private/password",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            parse(&args).unwrap().unwrap().password_file,
            PathBuf::from("/private/password")
        );
    }

    #[test]
    fn password_file_enforces_permissions_and_preserves_whitespace() {
        use std::os::unix::fs::PermissionsExt;
        let path =
            std::env::temp_dir().join(format!("estaciona-admin-test-{}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b" synthetic-password \r\n").unwrap();
        assert_eq!(read_password(&path).unwrap(), " synthetic-password ");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_password(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, vec![b'a'; 1025]).unwrap();
        assert!(read_password(&path).is_err());
        fs::write(&path, b"\n").unwrap();
        assert!(read_password(&path).is_err());
        fs::remove_file(&path).unwrap();
    }
}

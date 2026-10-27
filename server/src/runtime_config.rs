// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use crate::{lpr::LprMode, ws_origin::WsOriginPolicy};
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use std::{ffi::OsString, fmt, net::IpAddr, path::PathBuf, str::FromStr};

/// Shorter signing keys and peppers are refused at startup.
const MIN_SECRET_BYTES: usize = 32;
const MAX_SECRET_FILE_BYTES: u64 = 4096;

// Never derive Debug/Serialize: these settings contain credentials.
pub(crate) struct RuntimeConfig {
    pub site_data_dir: PathBuf,
    pub database: PgConnectOptions,
    pub jwt_secret: String,
    pub plate_pepper: String,
    pub personal_data_keys_file: PathBuf,
    pub identity: crate::identity::IdentityConfig,
    pub ws_origins: WsOriginPolicy,
    pub lpr_mode: LprMode,
    pub ws_limits: crate::ws_admission::WsLimits,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConfigError(&'static str);
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid or missing server setting: {}", self.0)
    }
}
impl std::error::Error for ConfigError {}

fn required(
    get: &mut impl FnMut(&str) -> Option<OsString>,
    key: &'static str,
) -> Result<String, ConfigError> {
    let text = get(key)
        .ok_or(ConfigError(key))?
        .into_string()
        .map_err(|_| ConfigError(key))?;
    if text.is_empty() {
        return Err(ConfigError(key));
    }
    Ok(text)
}

/// A secret comes from `KEY` or from the private file named by `KEY_FILE`, never both.
fn secret(
    get: &mut impl FnMut(&str) -> Option<OsString>,
    key: &'static str,
    file_key: &'static str,
) -> Result<String, ConfigError> {
    let Some(path) = get(file_key) else {
        return required(get, key);
    };
    if get(key).is_some() {
        return Err(ConfigError(file_key));
    }
    let text = crate::secret_file::read(std::path::Path::new(&path), MAX_SECRET_FILE_BYTES)
        .map_err(|_| ConfigError(file_key))?;
    if text.is_empty() {
        return Err(ConfigError(file_key));
    }
    Ok(text.to_string())
}

fn signing_secret(
    get: &mut impl FnMut(&str) -> Option<OsString>,
    key: &'static str,
    file_key: &'static str,
    too_short: &'static str,
) -> Result<String, ConfigError> {
    let value = secret(get, key, file_key)?;
    if value.len() < MIN_SECRET_BYTES {
        return Err(ConfigError(too_short));
    }
    Ok(value)
}

/// A local database (loopback or socket) may use any `sslmode`. A remote one
/// needs `sslmode=verify-full`, which checks the server's certificate and
/// name, unless `DATABASE_NETWORK=private` states that the path is a private
/// network (a container network): then `disable`, `require` and `verify-ca`
/// are also accepted. Opportunistic modes (`prefer`, `allow`) never are.
fn database(
    get: &mut impl FnMut(&str) -> Option<OsString>,
) -> Result<PgConnectOptions, ConfigError> {
    let url = secret(get, "DATABASE_URL", "DATABASE_URL_FILE")?;
    let options = PgConnectOptions::from_str(&url).map_err(|_| ConfigError("DATABASE_URL"))?;
    let private_network = match get("DATABASE_NETWORK") {
        None => false,
        Some(value) if value == "private" => true,
        Some(_) => {
            return Err(ConfigError(
                "DATABASE_NETWORK (only \"private\" is accepted)",
            ));
        }
    };
    let host = options
        .get_host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let local = options.get_socket().is_some()
        || host.starts_with('/')
        || host == "localhost"
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    let accepted = local
        || match options.get_ssl_mode() {
            PgSslMode::VerifyFull => true,
            PgSslMode::Disable | PgSslMode::Require | PgSslMode::VerifyCa => private_network,
            PgSslMode::Prefer | PgSslMode::Allow => false,
        };
    if !accepted {
        return Err(ConfigError(
            "DATABASE_URL (a remote database needs sslmode=verify-full, or DATABASE_NETWORK=private on a private container network)",
        ));
    }
    Ok(options)
}

/// An optional count from 1 to `max`.
fn count(
    get: &mut impl FnMut(&str) -> Option<OsString>,
    key: &'static str,
    default: usize,
    max: usize,
) -> Result<usize, ConfigError> {
    let Some(value) = get(key) else {
        return Ok(default);
    };
    value
        .to_str()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=max).contains(value))
        .ok_or(ConfigError(key))
}

/// Administrative commands need only the database.
pub(crate) fn database_from_env() -> Result<PgConnectOptions, ConfigError> {
    database(&mut |key| std::env::var_os(key))
}

impl RuntimeConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::parse(|key| std::env::var_os(key))
    }

    fn parse(mut value: impl FnMut(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
        let site_data_dir = required(&mut value, "SITE_DATA_DIR")?.into();
        let database = database(&mut value)?;
        let jwt_secret = signing_secret(
            &mut value,
            "JWT_SECRET",
            "JWT_SECRET_FILE",
            "JWT_SECRET (at least 32 bytes)",
        )?;
        let plate_pepper = signing_secret(
            &mut value,
            "PLATE_SECRET_PEPPER",
            "PLATE_SECRET_PEPPER_FILE",
            "PLATE_SECRET_PEPPER (at least 32 bytes)",
        )?;
        let personal_data_keys_file = required(&mut value, "PERSONAL_DATA_KEYS_FILE")?.into();
        let issuer = required(&mut value, "OIDC_ISSUER_URL")?;
        let client_id = required(&mut value, "OIDC_CLIENT_ID")?;
        let client_secret = secret(&mut value, "OIDC_CLIENT_SECRET", "OIDC_CLIENT_SECRET_FILE")?;
        let redirect = required(&mut value, "OIDC_REDIRECT_URL")?;
        let app_url = required(&mut value, "APP_URL")?;
        let identity = crate::identity::IdentityConfig::new(
            &issuer,
            &client_id,
            client_secret,
            &redirect,
            &app_url,
        )
        .map_err(ConfigError)?;
        let origins = required(&mut value, "WS_ALLOWED_ORIGINS")?;
        let ws_origins =
            WsOriginPolicy::parse(&origins).map_err(|_| ConfigError("WS_ALLOWED_ORIGINS"))?;
        let raw_lpr = value("LPR_ENABLED")
            .map(|v| v.into_string().map_err(|_| ConfigError("LPR_ENABLED")))
            .transpose()?;
        let lpr_mode =
            LprMode::parse(raw_lpr.as_deref()).map_err(|_| ConfigError("LPR_ENABLED"))?;
        let defaults = crate::ws_admission::WsLimits::default();
        let ws_limits = crate::ws_admission::WsLimits {
            // The pool has five connections: keep at least one for requests.
            handshakes: count(
                &mut value,
                "WS_HANDSHAKE_CONCURRENCY",
                defaults.handshakes,
                4,
            )?,
            edge: count(&mut value, "WS_MAX_EDGE_CONNECTIONS", defaults.edge, 10_000)?,
            app: count(&mut value, "WS_MAX_APP_CONNECTIONS", defaults.app, 100_000)?,
            dashboard: count(
                &mut value,
                "WS_MAX_DASHBOARD_CONNECTIONS",
                defaults.dashboard,
                10_000,
            )?,
        };
        Ok(Self {
            site_data_dir,
            database,
            jwt_secret,
            plate_pepper,
            personal_data_keys_file,
            identity,
            ws_origins,
            lpr_mode,
            ws_limits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, ffi::OsString};

    fn values() -> HashMap<&'static str, OsString> {
        [
            ("SITE_DATA_DIR", "/synthetic/deployment"),
            (
                "DATABASE_URL",
                "postgres://synthetic:password-not-for-logs@127.0.0.1/test",
            ),
            ("JWT_SECRET", "jwt-not-for-logs-0123456789abcdef"),
            (
                "PLATE_SECRET_PEPPER",
                "pepper-not-for-logs-0123456789abcdef",
            ),
            ("PERSONAL_DATA_KEYS_FILE", "/synthetic/personal-data.keys"),
            (
                "OIDC_ISSUER_URL",
                "https://auth.example.invalid/realms/estaciona",
            ),
            ("OIDC_CLIENT_ID", "synthetic-campus"),
            ("OIDC_CLIENT_SECRET", "oidc-secret-not-for-logs"),
            (
                "OIDC_REDIRECT_URL",
                "https://api.example.invalid/auth/callback",
            ),
            ("APP_URL", "https://app.example.invalid"),
            ("WS_ALLOWED_ORIGINS", "http://localhost"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.into()))
        .collect()
    }

    #[test]
    fn parses_current_settings_without_changing_secrets_and_defaults_lpr_off() {
        let mut data = values();
        data.insert("JWT_SECRET", "  significant secret spaces kept  ".into());
        let parsed = RuntimeConfig::parse(|key| data.get(key).cloned()).unwrap();
        assert_eq!(parsed.jwt_secret, "  significant secret spaces kept  ");
        assert_eq!(
            parsed.site_data_dir,
            std::path::PathBuf::from("/synthetic/deployment")
        );
        assert_eq!(parsed.lpr_mode, crate::lpr::LprMode::Disabled);
        data.insert("LPR_ENABLED", "true".into());
        assert_eq!(
            RuntimeConfig::parse(|key| data.get(key).cloned())
                .unwrap()
                .lpr_mode,
            crate::lpr::LprMode::Enabled
        );
    }

    #[test]
    fn missing_empty_and_non_utf8_settings_have_safe_field_only_errors() {
        use std::os::unix::ffi::OsStringExt;
        for key in [
            "SITE_DATA_DIR",
            "DATABASE_URL",
            "JWT_SECRET",
            "PLATE_SECRET_PEPPER",
            "PERSONAL_DATA_KEYS_FILE",
            "OIDC_ISSUER_URL",
            "OIDC_CLIENT_ID",
            "OIDC_CLIENT_SECRET",
            "OIDC_REDIRECT_URL",
            "APP_URL",
            "WS_ALLOWED_ORIGINS",
        ] {
            for value in [
                None,
                Some(OsString::new()),
                Some(OsString::from_vec(vec![255])),
            ] {
                let mut data = values();
                data.remove(key);
                if let Some(value) = value {
                    data.insert(key, value);
                }
                let error = RuntimeConfig::parse(|key| data.get(key).cloned())
                    .err()
                    .unwrap();
                assert!(error.to_string().contains(key));
                for secret in [
                    "jwt-not-for-logs",
                    "password-not-for-logs",
                    "pepper-not-for-logs",
                    "oidc-secret-not-for-logs",
                ] {
                    assert!(!format!("{error:?} {error}").contains(secret));
                }
            }
        }
        let mut data = values();
        data.insert("LPR_ENABLED", OsString::from_vec(vec![255]));
        assert!(RuntimeConfig::parse(|key| data.get(key).cloned()).is_err());
    }

    #[test]
    fn rejects_invalid_url_origins_and_lpr_without_echoing_the_input() {
        for (key, value) in [
            (
                "DATABASE_URL",
                "postgres://synthetic:do-not-echo@127.0.0.1:bad/test",
            ),
            ("WS_ALLOWED_ORIGINS", "https://do-not-echo.example/path"),
            ("LPR_ENABLED", "do-not-echo"),
            ("LPR_ENABLED", ""),
        ] {
            let mut data = values();
            data.insert(key, value.into());
            let error = RuntimeConfig::parse(|key| data.get(key).cloned())
                .err()
                .unwrap();
            assert!(!format!("{error:?} {error}").contains("do-not-echo"));
        }
    }

    fn private_file(contents: &str, mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("runtime-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("secret");
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn signing_secrets_shorter_than_32_bytes_are_refused() {
        for key in ["JWT_SECRET", "PLATE_SECRET_PEPPER"] {
            let mut data = values();
            data.insert(key, "s".repeat(31).into());
            let error = RuntimeConfig::parse(|key| data.get(key).cloned())
                .err()
                .unwrap();
            assert!(error.to_string().contains(key));
            assert!(error.to_string().contains("32 bytes"));
            assert!(!error.to_string().contains(&"s".repeat(31)));
            data.insert(key, "s".repeat(32).into());
            assert!(RuntimeConfig::parse(|key| data.get(key).cloned()).is_ok());
        }
    }

    #[test]
    fn secrets_can_come_from_private_files_but_not_from_both_sources() {
        let jwt = private_file(&format!("{}\n", "f".repeat(40)), 0o600);
        let url = private_file("postgres://synthetic:from-file@127.0.0.1/test\n", 0o600);
        let mut data = values();
        data.remove("JWT_SECRET");
        data.remove("DATABASE_URL");
        data.insert("JWT_SECRET_FILE", jwt.clone().into());
        data.insert("DATABASE_URL_FILE", url.into());
        let parsed = RuntimeConfig::parse(|key| data.get(key).cloned()).unwrap();
        assert_eq!(parsed.jwt_secret, "f".repeat(40));
        assert_eq!(parsed.database.get_username(), "synthetic");

        data.insert("JWT_SECRET", "e".repeat(40).into());
        let both = RuntimeConfig::parse(|key| data.get(key).cloned())
            .err()
            .unwrap();
        assert!(both.to_string().contains("JWT_SECRET_FILE"));

        data.remove("JWT_SECRET");
        let shared = private_file(&"g".repeat(40), 0o644);
        data.insert("JWT_SECRET_FILE", shared.into());
        let error = RuntimeConfig::parse(|key| data.get(key).cloned())
            .err()
            .unwrap();
        assert!(error.to_string().contains("JWT_SECRET_FILE"));
        assert!(!error.to_string().contains(&"g".repeat(40)));
    }

    #[test]
    fn remote_databases_need_verified_tls_or_a_declared_private_network() {
        let accepts = |url: &str, network: Option<&str>| {
            let mut data = values();
            data.insert("DATABASE_URL", url.into());
            if let Some(network) = network {
                data.insert("DATABASE_NETWORK", network.into());
            }
            RuntimeConfig::parse(|key| data.get(key).cloned()).is_ok()
        };
        // Local databases, any mode.
        for url in [
            "postgres://u:p@127.0.0.1/db",
            "postgres://u:p@localhost/db?sslmode=disable",
            "postgres://u:p@[::1]/db?sslmode=prefer",
            "postgres://u:p@localhost/db?host=/run/postgresql",
        ] {
            assert!(accepts(url, None), "{url}");
        }
        // Remote: only a verified certificate and name, unless declared private.
        let remote = |mode: &str| format!("postgres://u:p@db.internal/db?sslmode={mode}");
        assert!(accepts(&remote("verify-full"), None));
        for mode in ["disable", "require", "verify-ca", "prefer", "allow"] {
            assert!(!accepts(&remote(mode), None), "{mode}");
        }
        assert!(!accepts("postgres://u:p@10.0.0.5/db", None));
        for mode in ["disable", "require", "verify-ca", "verify-full"] {
            assert!(accepts(&remote(mode), Some("private")), "{mode}");
        }
        for mode in ["prefer", "allow"] {
            assert!(!accepts(&remote(mode), Some("private")), "{mode}");
        }
        assert!(!accepts(&remote("verify-full"), Some("public")));
    }

    #[test]
    fn websocket_limits_default_and_accept_only_bounded_counts() {
        let parse = |key: &'static str, value: &str| {
            let mut data = values();
            data.insert(key, value.into());
            RuntimeConfig::parse(|key| data.get(key).cloned())
        };
        let defaults = RuntimeConfig::parse(|key| values().get(key).cloned())
            .unwrap()
            .ws_limits;
        assert_eq!(defaults, crate::ws_admission::WsLimits::default());
        assert!(
            defaults.handshakes < 5,
            "keep pool connections for requests"
        );
        assert_eq!(
            parse("WS_MAX_EDGE_CONNECTIONS", "3")
                .unwrap()
                .ws_limits
                .edge,
            3
        );
        for (key, value) in [
            ("WS_HANDSHAKE_CONCURRENCY", "0"),
            ("WS_HANDSHAKE_CONCURRENCY", "5"),
            ("WS_MAX_EDGE_CONNECTIONS", "-1"),
            ("WS_MAX_APP_CONNECTIONS", "many"),
            ("WS_MAX_DASHBOARD_CONNECTIONS", "10001"),
        ] {
            assert_eq!(
                parse(key, value).err(),
                Some(ConfigError(key)),
                "{key}={value}"
            );
        }
    }

    #[test]
    fn identity_urls_need_https_except_on_this_machine() {
        let accepts = |key: &'static str, url: &str| {
            let mut data = values();
            data.insert(key, url.into());
            RuntimeConfig::parse(|key| data.get(key).cloned()).is_ok()
        };
        for key in ["OIDC_ISSUER_URL", "OIDC_REDIRECT_URL", "APP_URL"] {
            assert!(accepts(key, "http://localhost:8081/x"), "{key}");
            for url in [
                "http://auth.example.invalid/x",
                "https://auth.example.invalid/x?next=evil",
                "https://user:pass@auth.example.invalid/x",
                "not a url",
            ] {
                assert!(!accepts(key, url), "{key} {url}");
            }
        }
    }
}

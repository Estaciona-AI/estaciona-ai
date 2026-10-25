// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use crate::{lpr::LprMode, ws_origin::WsOriginPolicy};
use sqlx::postgres::PgConnectOptions;
use std::{ffi::OsString, fmt, path::PathBuf, str::FromStr};

// Never derive Debug/Serialize: these settings contain credentials.
pub(crate) struct RuntimeConfig {
    pub site_data_dir: PathBuf,
    pub database: PgConnectOptions,
    pub jwt_secret: String,
    pub plate_pepper: String,
    pub personal_data_keys_file: PathBuf,
    pub ws_origins: WsOriginPolicy,
    pub lpr_mode: LprMode,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ConfigError(&'static str);
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid or missing server setting: {}", self.0)
    }
}
impl std::error::Error for ConfigError {}

impl RuntimeConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::parse(|key| std::env::var_os(key))
    }

    fn parse(mut value: impl FnMut(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
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
        let site_data_dir = required(&mut value, "SITE_DATA_DIR")?.into();
        let url = required(&mut value, "DATABASE_URL")?;
        let database = PgConnectOptions::from_str(&url).map_err(|_| ConfigError("DATABASE_URL"))?;
        let jwt_secret = required(&mut value, "JWT_SECRET")?;
        let plate_pepper = required(&mut value, "PLATE_SECRET_PEPPER")?;
        let personal_data_keys_file = required(&mut value, "PERSONAL_DATA_KEYS_FILE")?.into();
        let origins = required(&mut value, "WS_ALLOWED_ORIGINS")?;
        let ws_origins =
            WsOriginPolicy::parse(&origins).map_err(|_| ConfigError("WS_ALLOWED_ORIGINS"))?;
        let raw_lpr = value("LPR_ENABLED")
            .map(|v| v.into_string().map_err(|_| ConfigError("LPR_ENABLED")))
            .transpose()?;
        let lpr_mode =
            LprMode::parse(raw_lpr.as_deref()).map_err(|_| ConfigError("LPR_ENABLED"))?;
        Ok(Self {
            site_data_dir,
            database,
            jwt_secret,
            plate_pepper,
            personal_data_keys_file,
            ws_origins,
            lpr_mode,
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
            ("JWT_SECRET", "jwt-not-for-logs"),
            ("PLATE_SECRET_PEPPER", "pepper-not-for-logs"),
            ("PERSONAL_DATA_KEYS_FILE", "/synthetic/personal-data.keys"),
            ("WS_ALLOWED_ORIGINS", "http://localhost"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.into()))
        .collect()
    }

    #[test]
    fn parses_current_settings_without_changing_secrets_and_defaults_lpr_off() {
        let mut data = values();
        data.insert("JWT_SECRET", "  significant secret spaces  ".into());
        let parsed = RuntimeConfig::parse(|key| data.get(key).cloned()).unwrap();
        assert_eq!(parsed.jwt_secret, "  significant secret spaces  ");
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
}

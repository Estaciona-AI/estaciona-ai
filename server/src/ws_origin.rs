// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use axum::http::{HeaderMap, StatusCode, Uri, header};
use std::{collections::HashSet, fmt};

#[derive(Clone, Debug)]
pub struct WsOriginPolicy {
    allowed: HashSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsOriginConfigError {
    EmptyList,
    EmptyEntry,
    InvalidOrigin,
    DuplicateOrigin,
}

impl fmt::Display for WsOriginConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyList => f.write_str("origin list is empty"),
            Self::EmptyEntry => f.write_str("origin list contains an empty entry"),
            Self::InvalidOrigin => f.write_str("origin list contains a non-canonical HTTP origin"),
            Self::DuplicateOrigin => f.write_str("origin list contains a duplicate"),
        }
    }
}

impl std::error::Error for WsOriginConfigError {}

impl WsOriginPolicy {
    pub fn parse(config: &str) -> Result<Self, WsOriginConfigError> {
        if config.trim().is_empty() {
            return Err(WsOriginConfigError::EmptyList);
        }

        let mut allowed = HashSet::new();
        for entry in config.split(',') {
            let origin = entry.trim();
            if origin.is_empty() {
                return Err(WsOriginConfigError::EmptyEntry);
            }
            if !is_canonical_http_origin(origin) {
                return Err(WsOriginConfigError::InvalidOrigin);
            }
            if !allowed.insert(origin.to_owned()) {
                return Err(WsOriginConfigError::DuplicateOrigin);
            }
        }

        Ok(Self { allowed })
    }

    /// The same list answers CORS for the browser apps of this installation.
    pub fn header_values(&self) -> Vec<axum::http::HeaderValue> {
        self.allowed
            .iter()
            .filter_map(|origin| axum::http::HeaderValue::from_str(origin).ok())
            .collect()
    }

    pub fn authorize(&self, headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
        let mut origins = headers.get_all(header::ORIGIN).iter();
        let Some(origin) = origins.next() else {
            return Err((StatusCode::FORBIDDEN, "Origin not allowed"));
        };
        if origins.next().is_some() {
            return Err((StatusCode::FORBIDDEN, "Origin not allowed"));
        }
        let Ok(origin) = origin.to_str() else {
            return Err((StatusCode::FORBIDDEN, "Origin not allowed"));
        };
        if !is_canonical_http_origin(origin) || !self.allowed.contains(origin) {
            return Err((StatusCode::FORBIDDEN, "Origin not allowed"));
        }
        Ok(())
    }
}

fn is_canonical_http_origin(origin: &str) -> bool {
    if origin.is_empty()
        || origin == "null"
        || origin.contains(['*', ',', '@'])
        || origin.bytes().any(|byte| byte.is_ascii_whitespace())
        || origin != origin.to_ascii_lowercase()
    {
        return false;
    }

    let Ok(uri) = origin.parse::<Uri>() else {
        return false;
    };
    let Some(scheme) = uri.scheme_str() else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let Some(authority) = uri.authority() else {
        return false;
    };
    if authority.host().is_empty()
        || (uri.path() != "" && uri.path() != "/")
        || uri.query().is_some()
        || origin != format!("{scheme}://{authority}")
    {
        return false;
    }
    // Credentials are refused above, so the authority is the host and an optional
    // port. A browser writes a port without leading zeros and omits the default.
    if let Some(port) = authority.as_str()[authority.host().len()..].strip_prefix(':') {
        let Ok(number) = port.parse::<u16>() else {
            return false;
        };
        if number == 0
            || port != number.to_string()
            || (scheme == "http" && number == 80)
            || (scheme == "https" && number == 443)
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{WsOriginConfigError, WsOriginPolicy, is_canonical_http_origin};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};

    #[test]
    fn authorization_requires_one_exact_configured_http_origin() {
        let policy =
            WsOriginPolicy::parse("https://parking.example.edu,http://localhost:5173").unwrap();
        let mut headers = HeaderMap::new();

        headers.insert(
            header::ORIGIN,
            "https://parking.example.edu".parse().unwrap(),
        );
        assert_eq!(policy.authorize(&headers), Ok(()));

        headers.insert(
            header::ORIGIN,
            "https://evil.parking.example.edu".parse().unwrap(),
        );
        assert_eq!(
            policy.authorize(&headers),
            Err((StatusCode::FORBIDDEN, "Origin not allowed"))
        );
        headers.insert(header::ORIGIN, "http://localhost:5173".parse().unwrap());
        assert_eq!(policy.authorize(&headers), Ok(()));
    }

    #[test]
    fn authorization_rejects_missing_null_malformed_and_multiple_origins() {
        let policy = WsOriginPolicy::parse("https://parking.example.edu").unwrap();
        let mut headers = HeaderMap::new();
        assert!(policy.authorize(&headers).is_err());

        headers.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(policy.authorize(&headers).is_err());
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://parking.example.edu/path"),
        );
        assert!(policy.authorize(&headers).is_err());
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://parking.example.edu"),
        );
        headers.append(
            header::ORIGIN,
            HeaderValue::from_static("https://parking.example.edu"),
        );
        assert!(policy.authorize(&headers).is_err());
    }

    #[test]
    fn configuration_requires_canonical_unique_http_origins() {
        assert_eq!(
            WsOriginPolicy::parse("").unwrap_err(),
            WsOriginConfigError::EmptyList
        );
        assert_eq!(
            WsOriginPolicy::parse("https://a.example,").unwrap_err(),
            WsOriginConfigError::EmptyEntry
        );
        assert_eq!(
            WsOriginPolicy::parse("*").unwrap_err(),
            WsOriginConfigError::InvalidOrigin
        );
        assert_eq!(
            WsOriginPolicy::parse("null").unwrap_err(),
            WsOriginConfigError::InvalidOrigin
        );
        assert_eq!(
            WsOriginPolicy::parse("https://a.example/path").unwrap_err(),
            WsOriginConfigError::InvalidOrigin
        );
        assert_eq!(
            WsOriginPolicy::parse("https://a.example/").unwrap_err(),
            WsOriginConfigError::InvalidOrigin
        );
        assert_eq!(
            WsOriginPolicy::parse("https://a.example:443").unwrap_err(),
            WsOriginConfigError::InvalidOrigin
        );
        assert_eq!(
            WsOriginPolicy::parse("https://a.example, https://a.example").unwrap_err(),
            WsOriginConfigError::DuplicateOrigin
        );
    }

    #[test]
    fn origins_that_parse_as_uris_must_still_be_canonical() {
        for origin in [
            "https://a.example",
            "http://localhost:5173",
            "https://a.example:80",
            "http://a.example:443",
            "https://127.0.0.1:8443",
            "https://[::1]:8443",
            "http://a.example:1",
            "http://a.example:65535",
        ] {
            assert!(is_canonical_http_origin(origin), "{origin}");
        }
        // Each of these is a valid URI, so only the canonical-form checks refuse it.
        for origin in [
            "https://user@a.example",
            "https://A.example",
            "HTTPS://a.example",
            "ftp://a.example",
            "https://a.example?next=1",
            "https://a.example/app",
            "http://a.example:80",
            "https://a.example:443",
        ] {
            assert!(!is_canonical_http_origin(origin), "{origin}");
        }
        // A browser never sends these ports, so an entry written this way would
        // start the server and then refuse its own app.
        for origin in [
            "https://a.example:08443",
            "http://localhost:05173",
            "https://[::1]:08443",
            "https://a.example:",
            "https://a.example:0",
            "https://a.example:65536",
            "https://a.example:99999",
            "https://a.example:+8443",
        ] {
            assert!(!is_canonical_http_origin(origin), "{origin}");
            assert_eq!(
                WsOriginPolicy::parse(origin).unwrap_err(),
                WsOriginConfigError::InvalidOrigin,
                "{origin}"
            );
        }
    }

    #[test]
    fn cors_answers_exactly_the_configured_origins() {
        let policy =
            WsOriginPolicy::parse("https://parking.example.edu,http://localhost:5173").unwrap();
        let mut values: Vec<_> = policy
            .header_values()
            .into_iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect();
        values.sort();
        assert_eq!(
            values,
            ["http://localhost:5173", "https://parking.example.edu"]
        );
        assert_eq!(
            WsOriginConfigError::InvalidOrigin.to_string(),
            "origin list contains a non-canonical HTTP origin"
        );
    }
}

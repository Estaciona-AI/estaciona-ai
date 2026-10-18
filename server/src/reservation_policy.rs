//! Installation-owned policy, passed explicitly to the reservation service.
//! A future tenant resolver can supply this value without transport-specific rules.
use serde::Deserialize;
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReservationPolicy {
    pub hold_seconds: i32,
    pub extension_seconds: i32,
    pub max_lifetime_seconds: i32,
    pub max_active_per_user: i64,
}

impl Default for ReservationPolicy {
    fn default() -> Self {
        Self {
            hold_seconds: 900,
            extension_seconds: 45,
            max_lifetime_seconds: 1800,
            max_active_per_user: 1,
        }
    }
}

impl ReservationPolicy {
    pub fn load(directory: &Path) -> io::Result<Self> {
        let content = match std::fs::read(directory.join("reservation_policy.json")) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let policy: Self = serde_json::from_slice(&content).map_err(io::Error::other)?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> io::Result<()> {
        if !(1..=86400).contains(&self.hold_seconds)
            || !(1..=86400).contains(&self.extension_seconds)
            || !(self.hold_seconds..=86400).contains(&self.max_lifetime_seconds)
            || !(1..=100).contains(&self.max_active_per_user)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid reservation policy limits",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_or_unbounded_policy_values() {
        for policy in [
            ReservationPolicy {
                hold_seconds: 0,
                ..Default::default()
            },
            ReservationPolicy {
                extension_seconds: -1,
                ..Default::default()
            },
            ReservationPolicy {
                max_lifetime_seconds: 899,
                ..Default::default()
            },
            ReservationPolicy {
                max_active_per_user: 0,
                ..Default::default()
            },
            ReservationPolicy {
                max_active_per_user: 101,
                ..Default::default()
            },
        ] {
            assert!(policy.validate().is_err());
        }
    }

    #[test]
    fn loads_installation_policy_and_rejects_malformed_configuration() {
        let root =
            std::env::temp_dir().join(format!("reservation-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        assert_eq!(
            ReservationPolicy::load(&root).unwrap().max_active_per_user,
            1
        );
        let file = root.join("reservation_policy.json");
        std::fs::write(&file, r#"{"max_active_per_user":3,"hold_seconds":300}"#).unwrap();
        assert_eq!(
            ReservationPolicy::load(&root).unwrap().max_active_per_user,
            3
        );
        std::fs::write(&file, r#"{"max_active_per_usr":3}"#).unwrap();
        assert!(ReservationPolicy::load(&root).is_err());
        std::fs::write(&file, r#"{"hold_seconds":-1}"#).unwrap();
        assert!(ReservationPolicy::load(&root).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}

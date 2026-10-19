// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::{
    fmt,
    fs::File,
    io::{self, Read},
    path::Path,
};

use serde::Deserialize;
use uuid::Uuid;

const MAX_DESCRIPTOR_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Production,
    Demo,
    Test,
}

/// Stable ownership coordinates; display names and slugs grant no authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallationIdentity {
    organization_id: Uuid,
    installation_id: Uuid,
    environment: Environment,
}

impl InstallationIdentity {
    pub fn organization_id(&self) -> Uuid {
        self.organization_id
    }
    pub fn installation_id(&self) -> Uuid {
        self.installation_id
    }
    pub fn environment(&self) -> Environment {
        self.environment
    }
}

#[derive(Debug)]
pub struct DescriptorEntity {
    id: Uuid,
    slug: String,
    name: String,
}

impl DescriptorEntity {
    pub fn id(&self) -> Uuid {
        self.id
    }
    pub fn slug(&self) -> &str {
        &self.slug
    }
    pub fn name(&self) -> &str {
        &self.name
    }

    fn validate(raw: RawEntity, entity: &'static str) -> Result<Self, DescriptorError> {
        let invalid = |field| DescriptorError::InvalidField { entity, field };
        let id = Uuid::parse_str(&raw.id).map_err(|_| invalid("id"))?;
        if id.is_nil() {
            return Err(invalid("id"));
        }
        if !(1..=63).contains(&raw.slug.len())
            || raw.slug.starts_with('-')
            || raw.slug.ends_with('-')
            || !raw
                .slug
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(invalid("slug"));
        }
        let name = raw.name.trim();
        if !(1..=128).contains(&name.chars().count()) {
            return Err(invalid("name"));
        }
        Ok(Self {
            id,
            slug: raw.slug,
            name: name.to_owned(),
        })
    }
}

/// Validated, explicit configuration. Loading never assigns an identity.
#[derive(Debug)]
pub struct InstallationDescriptor {
    schema_version: u32,
    organization: DescriptorEntity,
    installation: DescriptorEntity,
    environment: Environment,
}

impl InstallationDescriptor {
    pub fn load(path: &Path) -> Result<Self, DescriptorError> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take((MAX_DESCRIPTOR_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(DescriptorError::TooLarge);
        }
        let raw: RawDescriptor = serde_json::from_slice(&bytes).map_err(DescriptorError::Json)?;
        if raw.schema_version != 1 {
            return Err(DescriptorError::UnsupportedSchemaVersion(
                raw.schema_version,
            ));
        }
        Ok(Self {
            schema_version: raw.schema_version,
            organization: DescriptorEntity::validate(raw.organization, "organization")?,
            installation: DescriptorEntity::validate(raw.installation, "installation")?,
            environment: raw.environment,
        })
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
    pub fn organization(&self) -> &DescriptorEntity {
        &self.organization
    }
    pub fn installation(&self) -> &DescriptorEntity {
        &self.installation
    }
    pub fn identity(&self) -> InstallationIdentity {
        InstallationIdentity {
            organization_id: self.organization.id,
            installation_id: self.installation.id,
            environment: self.environment,
        }
    }
}

// Private wire structs cannot be used to bypass validation of public values.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDescriptor {
    schema_version: u32,
    organization: RawEntity,
    installation: RawEntity,
    environment: Environment,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntity {
    id: String,
    slug: String,
    name: String,
}

#[derive(Debug)]
pub enum DescriptorError {
    Io(io::Error),
    TooLarge,
    Json(serde_json::Error),
    UnsupportedSchemaVersion(u32),
    InvalidField {
        entity: &'static str,
        field: &'static str,
    },
}

impl From<io::Error> for DescriptorError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Cannot read installation descriptor: {error}"),
            Self::TooLarge => f.write_str("Installation descriptor exceeds 16 KiB"),
            Self::Json(error) => write!(
                f,
                "Invalid installation descriptor JSON at line {}, column {}",
                error.line(),
                error.column()
            ),
            Self::UnsupportedSchemaVersion(version) => write!(
                f,
                "Unsupported installation descriptor schema version: {version}"
            ),
            Self::InvalidField { entity, field } => {
                write!(f, "Invalid installation descriptor field: {entity}.{field}")
            }
        }
    }
}

impl std::error::Error for DescriptorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

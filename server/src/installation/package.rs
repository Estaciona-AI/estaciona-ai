// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::{InstallationDescriptor, InstallationIdentity};
use crate::{reservation_policy::ReservationPolicy, site_config::SiteConfig};
use std::{
    fmt,
    path::{Path, PathBuf},
};

#[derive(Debug, PartialEq, Eq)]
pub struct PackageError(&'static str);
impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid deployment package: {}", self.0)
    }
}
impl std::error::Error for PackageError {}

/// Validated local inputs. Loading does not connect, migrate or write files.
pub struct InstallationPackage {
    descriptor: InstallationDescriptor,
    site: SiteConfig,
    policy: ReservationPolicy,
}
impl fmt::Debug for InstallationPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallationPackage")
            .finish_non_exhaustive()
    }
}
impl InstallationPackage {
    pub fn load(directory: impl Into<PathBuf>) -> Result<Self, PackageError> {
        Self::from_manifest(&directory.into().join("installation.json"))
    }
    pub fn from_manifest(path: &Path) -> Result<Self, PackageError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = parent
            .canonicalize()
            .map_err(|_| PackageError("directory"))?;
        let name = path.file_name().ok_or(PackageError("installation.json"))?;
        let descriptor = InstallationDescriptor::load(&directory.join(name))
            .map_err(|_| PackageError("installation.json"))?;
        let site = SiteConfig::load(&directory)
            .map_err(|_| PackageError("maps (config.json / spots_3d.json)"))?;
        let policy = ReservationPolicy::load(&directory)
            .map_err(|_| PackageError("reservation_policy.json"))?;
        Ok(Self {
            descriptor,
            site,
            policy,
        })
    }
    pub fn identity(&self) -> InstallationIdentity {
        self.descriptor.identity()
    }
    pub fn directory(&self) -> &Path {
        &self.site.directory
    }
    pub(crate) fn into_parts(self) -> (SiteConfig, ReservationPolicy) {
        (self.site, self.policy)
    }

    /// Artificial maps/fixtures only; production must use the file loader.
    #[cfg(test)]
    pub(crate) fn for_unit_test(
        identity: InstallationIdentity,
        site: SiteConfig,
        policy: ReservationPolicy,
    ) -> Self {
        Self {
            descriptor: super::test_support::descriptor(identity),
            site,
            policy,
        }
    }
}

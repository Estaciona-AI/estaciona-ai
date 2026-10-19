// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Pure installation descriptor types; database binding and startup are separate.

mod descriptor;

pub use descriptor::{
    DescriptorEntity, DescriptorError, Environment, InstallationDescriptor, InstallationIdentity,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod binding_tests;

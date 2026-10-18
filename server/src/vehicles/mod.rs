// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

mod api;
mod store;

pub use api::routes;
pub use store::{approved_user, request_binding};

#[cfg(test)]
mod tests;

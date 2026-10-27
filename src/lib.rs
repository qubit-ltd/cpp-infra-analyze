// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Governed C++ static analysis.

mod application;
mod clang_runtime;
mod clang_tidy;
mod cli_options;
mod compilation_database;
mod configuration;
mod configuration_evidence;
mod diagnostics;
mod digest;
mod error;
mod native_token;
mod nolint;
mod preparation;
mod process;
mod reporting;
mod sarif;
mod source_exclusions;
mod source_text;
#[cfg(test)]
mod tests;

pub use crate::application::cli;
pub use crate::error::Error;

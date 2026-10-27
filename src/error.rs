// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Typed failures and process exit classification.

/// Failure returned by analysis setup, execution, or report persistence.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A configuration, protocol, platform, or unsupported capability error.
    #[error("{0}")]
    Configuration(String),
    /// A required check or build failed.
    #[error("{0}")]
    Gate(String),
    /// A file or child process could not be accessed.
    #[error("{0}")]
    Infrastructure(String),
    /// The user cancelled the running process tree.
    #[error("analysis cancelled")]
    Cancelled,
}

impl Error {
    /// Returns the shared cpp-infra process exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Configuration(_) => 2,
            Self::Gate(_) => 1,
            Self::Infrastructure(_) => 3,
            Self::Cancelled => 130,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Infrastructure(error.to_string())
    }
}

pub(crate) fn config(error: impl std::fmt::Display) -> Error {
    Error::Configuration(error.to_string())
}

// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Read supported C/C++ text without silently dropping unreadable inputs.
use std::fs;
use std::path::Path;

use crate::error::Error;
use crate::error::config;

pub(crate) fn read(path: &Path) -> Result<String, Error> {
    let bytes = fs::read(path)
        .map_err(|error| Error::Infrastructure(format!("cannot read analysis input {}: {error}", path.display())))?;
    String::from_utf8(bytes).map_err(|error| {
        config(format!(
            "analysis input {} uses unsupported encoding; expected UTF-8: {error}",
            path.display()
        ))
    })
}

pub(crate) fn is_code(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()).is_some_and(|extension| {
        [
            "c", "C", "cc", "cpp", "cxx", "m", "mm", "h", "hh", "hpp", "hxx", "inc", "ipp", "tpp",
        ]
        .contains(&extension)
    })
}

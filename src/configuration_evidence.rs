// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! File ownership and content evidence supplied by authoritative verify
//! prepare.
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ConfigurationEvidence {
    pub path: PathBuf,
    pub sha256: String,
    pub kind: String,
}

// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Physical spelling ranges produced by Clang's stable lexical C API.

pub(crate) struct NativeToken {
    pub kind: u32,
    pub start: usize,
    pub end: usize,
}

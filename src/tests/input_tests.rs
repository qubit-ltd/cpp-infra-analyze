// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use crate::clang_runtime::ClangRuntime;
use crate::configuration;
use crate::digest;
use crate::error::Error;
use crate::native_token::NativeToken;
use crate::nolint;
use crate::source_text;

#[test]
fn test_missing_inputs_preserve_path_and_error_class() {
    let temp = tempfile::tempdir().expect("temporary project");
    let missing = temp.path().join("missing.cpp");
    let error = source_text::read(&missing).expect_err("read failure must not mean empty source");
    assert!(matches!(error, Error::Infrastructure(_)));
    assert!(error.to_string().contains("missing.cpp"));
    let error = configuration::load::<serde_json::Value>(&missing).expect_err("missing config");
    assert!(matches!(error, Error::Configuration(_)));
    assert!(error.to_string().contains("missing.cpp"));
    let error = configuration::inside(temp.path(), Path::new("missing.cpp")).expect_err("missing input");
    assert!(error.to_string().contains("missing.cpp"));
}

#[test]
fn test_non_utf8_paths_are_rejected_by_each_input_protocol() {
    for config_only in [false, true] {
        let temp = tempfile::tempdir().expect("temporary project");
        let path = temp.path().join(OsString::from_vec(b"input-\xff.cpp".to_vec()));
        fs::write(&path, "int clean();\n").expect("non-UTF8 filename");
        let error = digest::project(temp.path(), config_only, &[]).expect_err("digest filename must be UTF8");
        assert!(error.to_string().contains("non UTF-8 input path"));
        let runtime = ClangRuntime::load().expect("required real Clang runtime");
        let error = match runtime.tokens(&path, "int clean();\n") {
            Ok(_) => panic!("native protocol cannot encode this filename"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("lexer input path must be UTF-8"));
    }
}

#[test]
fn test_uncovered_or_unknown_native_tokens_cannot_certify_suppressions() {
    let error = nolint::positions("// NOLINT", &[]).expect_err("missing native range");
    assert!(error.to_string().contains("no containing token"));
    let token = NativeToken {
        kind: 0,
        start: 0,
        end: 9,
    };
    let error = nolint::positions("// NOLINT", &[token]).expect_err("unsupported native classification");
    assert!(error.to_string().contains("unsupported Clang token kind 0"));
}

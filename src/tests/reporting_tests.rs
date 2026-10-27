// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
use std::path::Path;

use serde_json::Value;

use crate::reporting::Run;
use crate::reporting::validate_owned_path;
use crate::reporting::validate_report_destination;

#[test]
fn test_report_internal_identity_and_artifact_ownership_fail_closed() {
    let temp = tempfile::tempdir().expect("temporary project");
    let project = temp.path().canonicalize().expect("canonical project");
    let mut run = Run::new(&project, None, None).expect("run");
    let error = run
        .artifact("outside", &project.join("business.json"), true)
        .expect_err("artifact outside report root");
    assert!(error.to_string().contains("artifact is outside report_root"));
    run.value["project"] = Value::Null;
    let error = run.finish(&Ok(())).expect_err("missing internal project identity");
    assert!(error.to_string().contains("report project missing"));
    let error = validate_owned_path(&project, Path::new("/outside")).expect_err("outside managed output");
    assert!(error.to_string().contains("outside project"));
    let error = validate_report_destination(&project, Path::new("")).expect_err("empty destination");
    assert!(error.to_string().contains("invalid output path"));
}

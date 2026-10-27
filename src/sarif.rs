// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! SARIF 2.1.0 export with relative artifact locations and stable fingerprints.
use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use serde_json::json;

use crate::digest;
use crate::error::Error;
use crate::error::config;
use crate::reporting::atomic;

fn uri(path: &str) -> String {
    let mut result = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

pub(crate) fn write(path: &Path, diagnostics: &[Value]) -> Result<(), Error> {
    let rules: BTreeSet<_> = diagnostics.iter().filter_map(|d| d["rule"].as_str()).collect();
    let mut results = Vec::new();
    for diagnostic in diagnostics {
        let stable = serde_json::to_vec(&json!([
            diagnostic["rule"],
            diagnostic["project_relative_path"],
            diagnostic["line"],
            diagnostic["column"],
            diagnostic["message"],
            diagnostic["macro_context"]
        ]))
        .map_err(config)?;
        let mut result = json!({"ruleId":diagnostic["rule"],"level":match diagnostic["severity"].as_str() {Some("error")=>"error",Some("warning")=>"warning",_=>"note"},"message":{"text":diagnostic["message"]},"partialFingerprints":{"cpp-infra/v1":digest::bytes(&stable)}});
        if let Some(relative) = diagnostic["project_relative_path"].as_str() {
            let mut physical = json!({"artifactLocation":{"uri":uri(relative),"uriBaseId":"%SRCROOT%"}});
            if diagnostic["line"].is_number() {
                physical["region"] = json!({"startLine":diagnostic["line"],"startColumn":diagnostic["column"]});
            }
            result["locations"] = json!([{"physicalLocation":physical}]);
        }
        if let Some(reason) = diagnostic["suppression_reason"].as_str() {
            result["suppressions"] = json!([{"kind":"external","status":"accepted","justification":reason}]);
        }
        results.push(result);
    }
    let sarif = json!({"$schema":"https://json.schemastore.org/sarif-2.1.0.json","version":"2.1.0","runs":[{"tool":{"driver":{"name":"cpp-infra-analyze","version":env!("CARGO_PKG_VERSION"),"rules":rules.into_iter().map(|id|json!({"id":id})).collect::<Vec<_>>()}},"results":results}]});
    atomic(path, &serde_json::to_vec_pretty(&sarif).map_err(config)?)
}

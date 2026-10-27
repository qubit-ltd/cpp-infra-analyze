// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Validate exact report-file exclusions without hiding project inputs.
use std::collections::BTreeSet;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use crate::configuration::Project;
use crate::configuration::inside;
use crate::configuration::load_input;
use crate::error::Error;
use crate::error::config;
use crate::reporting::validate_report_destination;

pub(crate) fn validate(project: &Path, paths: &[String]) -> Result<Vec<PathBuf>, Error> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    for relative in paths {
        validate_raw(relative)?;
    }
    let roots: Project = load_input(project, Path::new(".infra/project.toml"))?;
    let mut result = BTreeSet::new();
    for relative in paths {
        let path = Path::new(relative);
        let first = relative
            .split('/')
            .next()
            .ok_or_else(|| config("empty source exclusion"))?;
        if crate::reporting::reserved_project_input(path) {
            return Err(config(format!(
                "source exclusion cannot hide reserved project input: {relative}"
            )));
        }
        let absolute = project.join(path);
        let resolved = validate_report_destination(project, &absolute)
            .map_err(|error| config(format!("invalid source exclusion {relative}: {error}")))?;
        if resolved != absolute || resolved.is_dir() {
            return Err(config(format!(
                "source exclusion is not a canonical report file: {relative}"
            )));
        }
        if !["build", "target"].contains(&first) {
            for root in roots
                .source_roots
                .iter()
                .chain(&roots.test_roots)
                .chain(&roots.generated_roots)
                .chain(&roots.third_party_roots)
            {
                if absolute.starts_with(inside(project, root)?) {
                    return Err(config(format!(
                        "source exclusion cannot hide governed input: {relative}"
                    )));
                }
            }
        }
        if absolute.exists() {
            let report: Value = serde_json::from_slice(&fs::read(&absolute)?)
                .map_err(|e| config(format!("source exclusion is not a public report: {relative}: {e}")))?;
            if report["schema"] != 1
                || !report["tool"]["name"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("cpp-infra-"))
                || !report["tool"]["version"].is_string()
                || !report["status"].is_string()
                || !report["commands"].is_array()
                || !report["diagnostics"].is_array()
                || !report["artifacts"].is_array()
            {
                return Err(config(format!(
                    "source exclusion is not a public cpp-infra report: {relative}"
                )));
            }
        }
        result.insert(absolute);
    }
    Ok(result.into_iter().collect())
}

fn validate_raw(relative: &str) -> Result<(), Error> {
    let path = Path::new(relative);
    let normalized = path
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("/"));
    let drive_prefix =
        relative.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) && relative.as_bytes().get(1) == Some(&b':');
    if relative.is_empty()
        || relative.contains('\\')
        || drive_prefix
        || normalized.as_deref() != Some(relative)
        || path.extension().and_then(|s| s.to_str()) != Some("json")
    {
        return Err(config(format!("invalid source exclusion report path: {relative}")));
    }
    Ok(())
}

// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Strict project, analysis, policy, and exception configuration.
use std::collections::BTreeMap;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::Error;
use crate::error::config;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Configuration {
    pub schema: u32,
    pub profile: String,
    pub targets: Vec<String>,
    #[serde(default)]
    pub include_tests: bool,
    #[serde(default = "default_jobs")]
    pub jobs: usize,
    #[serde(default = "default_timeout")]
    pub timeout_per_translation_unit_seconds: u64,
    pub exceptions: Option<PathBuf>,
    #[serde(default)]
    pub additive_checks: Vec<String>,
}

fn default_jobs() -> usize {
    1
}
fn default_timeout() -> u64 {
    180
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Project {
    pub schema: u32,
    pub kind: String,
    pub source_roots: Vec<PathBuf>,
    #[serde(default)]
    pub test_roots: Vec<PathBuf>,
    #[serde(default)]
    pub generated_roots: Vec<PathBuf>,
    #[serde(default)]
    pub third_party_roots: Vec<PathBuf>,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub configure: String,
    pub build: String,
    pub test: Option<String>,
    pub conan_context: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    pub schema: u32,
    pub mandatory_checks: Vec<String>,
    #[serde(default)]
    pub exception_rules: Vec<String>,
    pub reject_nolint: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Exceptions {
    pub schema: u32,
    #[serde(default)]
    pub exceptions: Vec<Exception>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Exception {
    pub rule: String,
    pub path: String,
    pub reason: String,
    pub message_digest: Option<String>,
}

pub(crate) fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    toml::from_str(&fs::read_to_string(path).map_err(|e| config(format!("{}: {e}", path.display())))?)
        .map_err(|e| config(format!("{}: {e}", path.display())))
}

pub(crate) fn tracked_input(project: &Path, relative: &Path) -> Result<PathBuf, Error> {
    if relative.is_absolute() || relative.components().any(|part| matches!(part, Component::ParentDir)) {
        return Err(config(format!(
            "configuration input must be project relative: {}",
            relative.display()
        )));
    }
    let excluded = |path: &Path| {
        let parts: Vec<_> = path.components().map(|part| part.as_os_str()).collect();
        parts
            .first()
            .is_some_and(|part| [".git", "build", "target"].iter().any(|name| *part == *name))
            || parts.first().is_some_and(|part| *part == ".infra")
                && parts.get(1).is_some_and(|part| {
                    [".tmp", "transactions", ".transactions"]
                        .iter()
                        .any(|name| *part == *name)
                })
    };
    if excluded(relative) {
        return Err(config(format!(
            "configuration input is in a digest-excluded zone: {}",
            relative.display()
        )));
    }
    let resolved = inside(project, relative)?;
    let canonical_relative = resolved.strip_prefix(project).map_err(config)?;
    if excluded(canonical_relative) {
        return Err(config(format!(
            "configuration input resolves into a digest-excluded zone: {} -> {}",
            relative.display(),
            canonical_relative.display()
        )));
    }
    if !resolved.is_file() {
        return Err(config(format!(
            "configuration input must be a regular file: {}",
            relative.display()
        )));
    }
    Ok(resolved)
}

pub(crate) fn load_input<T: serde::de::DeserializeOwned>(project: &Path, relative: &Path) -> Result<T, Error> {
    load(&tracked_input(project, relative)?)
}

pub(crate) fn validate_inputs(project: &Path, inputs: &[PathBuf]) -> Result<(), Error> {
    for input in inputs {
        tracked_input(project, input)?;
    }
    Ok(())
}

pub(crate) fn inside(project: &Path, relative: &Path) -> Result<PathBuf, Error> {
    if relative.is_absolute() || relative.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(config(format!("path must be project relative: {}", relative.display())));
    }
    let path = project
        .join(relative)
        .canonicalize()
        .map_err(|e| config(format!("{}: {e}", relative.display())))?;
    if !path.starts_with(project) {
        return Err(config(format!("path escapes project: {}", relative.display())));
    }
    Ok(path)
}

pub(crate) fn validate(
    project: &Path,
    settings: &Configuration,
    roots: &Project,
    policy: &Policy,
) -> Result<(), Error> {
    if settings.schema != 1 || roots.schema != 1 || policy.schema != 1 {
        return Err(config("unsupported configuration schema"));
    }
    if !["library", "application", "header-only"].contains(&roots.kind.as_str()) {
        return Err(config("invalid project kind"));
    }
    if settings.targets.is_empty()
        || settings.jobs == 0
        || settings.jobs > 256
        || settings.timeout_per_translation_unit_seconds == 0
    {
        return Err(config("targets, jobs and timeout must be nonempty and positive"));
    }
    let mut targets = settings.targets.clone();
    targets.sort();
    targets.dedup();
    if targets.len() != settings.targets.len() {
        return Err(config("duplicate analysis targets"));
    }
    let mut paths = Vec::new();
    for root in roots
        .source_roots
        .iter()
        .chain(&roots.test_roots)
        .chain(&roots.generated_roots)
        .chain(&roots.third_party_roots)
    {
        let path = inside(project, root)?;
        if !path.is_dir() {
            return Err(config("project roots must be directories"));
        }
        if paths
            .iter()
            .any(|existing: &PathBuf| path.starts_with(existing) || existing.starts_with(&path))
        {
            return Err(config("overlapping project roots"));
        }
        paths.push(path);
    }
    if roots.source_roots.is_empty() {
        return Err(config("source_roots is empty"));
    }
    if policy.mandatory_checks.is_empty() {
        return Err(config("mandatory_checks is empty"));
    }
    for check in settings.additive_checks.iter().chain(&policy.mandatory_checks) {
        if check.is_empty()
            || check.starts_with('-')
            || !check.chars().all(|c| c.is_ascii_alphanumeric() || "-_*".contains(c))
        {
            return Err(config(format!("invalid additive/mandatory check: {check}")));
        }
    }
    for profile in roots.profiles.values() {
        if profile.configure.is_empty()
            || profile.build.is_empty()
            || profile.test.as_ref().is_some_and(String::is_empty)
            || profile.conan_context.as_ref().is_some_and(String::is_empty)
        {
            return Err(config("empty profile preset or Conan context"));
        }
    }
    Ok(())
}

// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Verify preparation and strict validation of the resulting build identity.
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;

use fs2::FileExt;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::configuration::Profile;
use crate::configuration_evidence::ConfigurationEvidence;
use crate::digest;
use crate::error::Error;
use crate::error::config;
use crate::process;
use crate::reporting::Run;
use crate::reporting::atomic;

#[derive(Debug, Deserialize)]
pub(crate) struct BuildContext {
    pub schema: u32,
    pub project: PathBuf,
    pub profile: String,
    pub binary_dir: PathBuf,
    pub configure_preset: String,
    pub build_preset: String,
    pub source_digest: String,
    pub config_digest: String,
    #[serde(default)]
    pub source_excluded_paths: Vec<String>,
    pub fingerprint: String,
    pub configuration_inputs: Vec<ConfigurationEvidence>,
    pub cmake_cache_digest: String,
    pub compiler: Compiler,
    pub generator: String,
    pub compile_commands: Option<PathBuf>,
    pub targets: Vec<Target>,
    #[serde(default)]
    pub header_translation_units: Vec<Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Compiler {
    pub id: String,
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Target {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub sources: Vec<PathBuf>,
    pub artifacts: Vec<PathBuf>,
}

pub(crate) fn prepare(
    project: &Path,
    profile: &str,
    declaration: &Profile,
    run: &mut Run,
    offline: bool,
) -> Result<(BuildContext, File), Error> {
    let context_path = run.root.join("analysis-build-context.json");
    let child_context = run.root.join("verify-run-context.json");
    let mut child = json!({"schema":1,"run_id":run.value["run_id"],"task_id":"analyze-prepare","bundle_id":run.value["bundle_id"],"project":project,"profile":profile,"source_digest":run.source_digest,"config_digest":run.config_digest,"report_root":run.report_root,"source_excluded_paths":run.source_excluded_paths});
    if let Some(path) = &run.prepared_context {
        child["build_context"] = json!(path);
    }
    if let Some((path, digest)) = &run.runtime_snapshot {
        child["runtime_snapshot"] = json!(path);
        child["runtime_snapshot_digest"] = json!(digest);
    }
    atomic(&child_context, &serde_json::to_vec_pretty(&child).map_err(config)?)?;
    let verify = process::discover("cpp-infra-verify", true)?;
    let mut args = vec![
        "--project".into(),
        project.display().to_string(),
        "--report".into(),
        run.root.join("verify-report.json").display().to_string(),
        "--run-context".into(),
        child_context.display().to_string(),
    ];
    if offline {
        args.push("--offline".into());
    }
    args.extend([
        "prepare".into(),
        "--profile".into(),
        profile.into(),
        "--output".into(),
        context_path.display().to_string(),
    ]);
    let result = process::run(&verify, &args, project, &run.root.join("verify.log"), 900)?;
    run.command(result.record);
    if result.cancelled {
        return Err(Error::Cancelled);
    }
    if result.timed_out {
        return Err(Error::Gate("verify preparation timed out".into()));
    }
    match result.code {
        Some(0) => {}
        Some(1) => {
            return Err(Error::Gate("verify preparation failed; see verify.log".into()));
        }
        Some(130) => return Err(Error::Cancelled),
        Some(2) => return Err(config("verify rejected preparation configuration")),
        _ => {
            return Err(Error::Infrastructure("verify preparation process failed".into()));
        }
    }
    let context: BuildContext = serde_json::from_slice(&fs::read(&context_path)?).map_err(config)?;
    if context.schema != 1
        || context.project.canonicalize()? != project
        || context.profile != profile
        || context.configure_preset != declaration.configure
        || context.build_preset != declaration.build
        || context.fingerprint.is_empty()
    {
        return Err(config("verify build-context identity/profile/preset mismatch"));
    }
    if context.source_digest != run.source_digest
        || context.config_digest != run.config_digest
        || context.source_excluded_paths != run.source_excluded_paths
    {
        return Err(config("stale verify build-context input digests"));
    }
    let binary = context.binary_dir.canonicalize()?;
    if !binary.starts_with(project.join("build/infra")) || binary == project.join("build/infra") {
        return Err(config(
            "build-context binary_dir must be an exclusive build/infra directory",
        ));
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(binary.join(".cpp-infra.lock"))?;
    let waiting = std::time::Instant::now();
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if crate::process::CANCELLED.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(Error::Cancelled);
                }
                if waiting.elapsed().as_secs() >= 900 {
                    return Err(Error::Infrastructure(
                        "timed out waiting for analysis build lock".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
    validate_consumption_state(project, &context, run)?;
    let current: Value = serde_json::from_slice(&fs::read(&context_path)?).map_err(config)?;
    if current["fingerprint"] != context.fingerprint {
        return Err(config("inputs changed after prepare"));
    }
    if context.generator != "Ninja"
        || context.compiler.id != "Clang"
        || !context.compiler.version.starts_with("18.")
        || !context.compiler.path.is_absolute()
        || !context.compiler.path.is_file()
    {
        return Err(config(
            "analysis requires Linux Clang 18 and single-configuration Ninja",
        ));
    }
    let cache = fs::read_to_string(binary.join("CMakeCache.txt"))?;
    let setting = |name: &str| {
        cache.lines().find_map(|line| {
            line.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix(':'))
                .and_then(|rest| rest.split_once('=').map(|(_, value)| value))
        })
    };
    if setting("CMAKE_UNITY_BUILD").is_some_and(|s| ["ON", "1", "TRUE", "YES"].contains(&s)) {
        return Err(config("Unity build cannot be analyzed"));
    }
    if !setting("CMAKE_EXPORT_COMPILE_COMMANDS").is_some_and(|s| ["ON", "1", "TRUE", "YES"].contains(&s)) {
        return Err(config("CMAKE_EXPORT_COMPILE_COMMANDS must be enabled"));
    }
    if setting("CMAKE_GENERATOR") != Some("Ninja") || setting("CMAKE_HOME_DIRECTORY").map(Path::new) != Some(project) {
        return Err(config("CMake cache generator/source root mismatch"));
    }
    let actual_compiler = setting("CMAKE_CXX_COMPILER").ok_or_else(|| config("CMake cache has no C++ compiler"))?;
    if Path::new(actual_compiler).canonicalize()? != context.compiler.path.canonicalize()? {
        return Err(config("CMake cache/build-context compiler mismatch"));
    }
    let database = context
        .compile_commands
        .as_ref()
        .ok_or_else(|| config("build context has no compilation database"))?
        .canonicalize()?;
    if !database.starts_with(&binary) {
        return Err(config("compilation database escapes binary_dir"));
    }
    for target in &context.targets {
        if target.name.is_empty()
            || target.kind.is_empty()
            || target.sources.iter().any(|p| !p.is_absolute())
            || target.artifacts.iter().any(|p| !p.is_absolute())
        {
            return Err(config("invalid File API target identity/artifact"));
        }
    }
    run.artifact("build-context", &context_path, true)?;
    run.value["data"]["build_fingerprint"] = json!(context.fingerprint);
    run.value["external_versions"]["compiler"] =
        json!({"id":context.compiler.id,"path":context.compiler.path,"version":context.compiler.version});
    run.value["external_versions"]["generator"] = json!(context.generator);
    run.value["data"]["verify_header_translation_units"] = json!(context.header_translation_units.len());
    Ok((context, lock))
}

/// Rechecks the complete build identity while the caller holds its consumption
/// lock. Preparation and final freshness use the same barrier; divergent state,
/// proof, cache, or project inputs are configuration errors and read failures
/// preserve their infrastructure error.
pub(crate) fn validate_consumption_state(project: &Path, context: &BuildContext, run: &Run) -> Result<(), Error> {
    let binary = context.binary_dir.canonicalize()?;
    crate::configuration::validate_inputs(project, &run.configuration_inputs)?;
    let marker: Value = serde_json::from_slice(&fs::read(binary.join(".cpp-infra-project.json"))?).map_err(config)?;
    let state: Value =
        serde_json::from_slice(&fs::read(binary.join(".cpp-infra-build-state.json"))?).map_err(config)?;
    let state_exclusions: Vec<String> =
        serde_json::from_value(state.get("source_excluded_paths").cloned().unwrap_or_else(|| json!([])))
            .map_err(config)?;
    validate_configuration_proof(context)?;
    if state_exclusions != run.source_excluded_paths
        || marker["schema"] != 1
        || marker["project"] != project.to_string_lossy().as_ref()
        || state["schema"] != 1
        || state["project"] != project.to_string_lossy().as_ref()
        || state["profile"] != context.profile
        || state["source_digest"] != run.source_digest
        || state["config_digest"] != run.config_digest
        || state["fingerprint"] != context.fingerprint
        || state["configuration_inputs"] != serde_json::to_value(&context.configuration_inputs).map_err(config)?
        || state["cmake_cache_digest"] != context.cmake_cache_digest
    {
        return Err(config(
            "build directory management marker/state mismatch; another prepare may have changed the build",
        ));
    }
    if digest::project(project, false, &run.excluded)? != run.source_digest
        || digest::project(project, true, &run.excluded)? != run.config_digest
    {
        return Err(config("project inputs changed after preparation or during analysis"));
    }
    Ok(())
}

/// Closes the race after authoritative prepare and before/following
/// consumption. Ownership and fingerprint construction remain exclusively
/// verify's job.
pub(crate) fn validate_configuration_proof(context: &BuildContext) -> Result<(), Error> {
    let valid_sha = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if context.configuration_inputs.is_empty() || !valid_sha(&context.cmake_cache_digest) {
        return Err(config("build-context lacks configuration/cache evidence"));
    }
    for input in &context.configuration_inputs {
        if !input.path.is_absolute()
            || input.path.to_str().is_none()
            || input.path.canonicalize()? != input.path
            || !valid_sha(&input.sha256)
            || !["project", "dependency-generated", "cmake-generated", "cmake-tool"].contains(&input.kind.as_str())
        {
            return Err(config(format!(
                "invalid build configuration input evidence: {}",
                input.path.display()
            )));
        }
        if digest::bytes(&fs::read(&input.path)?) != input.sha256 {
            return Err(config(format!(
                "build configuration input changed after preparation: {}",
                input.path.display()
            )));
        }
    }
    let binary = context.binary_dir.canonicalize()?;
    let cache = binary.join("CMakeCache.txt");
    if cache.canonicalize()? != cache || digest::bytes(&fs::read(&cache)?) != context.cmake_cache_digest {
        return Err(config("CMake cache digest changed after preparation"));
    }
    Ok(())
}

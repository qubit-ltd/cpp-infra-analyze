// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Atomic common reports and content-addressed artifacts.
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::digest;
use crate::error::Error;
use crate::error::config;

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Run {
    pub root: PathBuf,
    pub report_root: PathBuf,
    pub report: PathBuf,
    pub value: Value,
    pub started: Instant,
    pub source_digest: String,
    pub config_digest: String,
    pub excluded: Vec<PathBuf>,
    pub runtime_snapshot: Option<(PathBuf, String)>,
    pub prepared_context: Option<PathBuf>,
    pub source_excluded_paths: Vec<String>,
    pub configuration_inputs: Vec<PathBuf>,
}

pub(crate) fn version() -> Value {
    json!({"name":"cpp-infra-analyze","version":env!("CARGO_PKG_VERSION"),"revision":option_env!("CPP_INFRA_BUILD_REVISION"),"protocol":1})
}

pub(crate) fn atomic(path: &Path, content: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let temp = path.with_file_name(name);
    fs::write(&temp, content)?;
    fs::rename(&temp, path)?;
    Ok(())
}

pub(crate) fn validate_owned_path(project: &Path, path: &Path) -> Result<(), Error> {
    let relative = path
        .strip_prefix(project)
        .map_err(|_| config("managed output path outside project"))?;
    let mut ancestor = project.to_owned();
    for component in relative.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(config("managed output contains parent traversal"));
        }
        ancestor.push(component);
        match ancestor.symlink_metadata() {
            Ok(_) => {
                if !ancestor.canonicalize()?.starts_with(project) {
                    return Err(config("managed output symlink escapes project"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn validate_report_destination(project: &Path, path: &Path) -> Result<PathBuf, Error> {
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(config("output contains parent traversal"));
    }
    if path.starts_with(project) {
        validate_owned_path(project, path)?;
    }
    let mut ancestor = path.to_owned();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .ok_or_else(|| config("invalid output path"))?
                .to_owned(),
        );
        if !ancestor.pop() {
            return Err(config("output has no existing parent"));
        }
    }
    let mut resolved = ancestor.canonicalize()?;
    for name in suffix.iter().rev() {
        resolved.push(name);
    }
    if resolved.starts_with(project) {
        let relative = resolved.strip_prefix(project).map_err(config)?;
        if reserved_project_input(relative) {
            return Err(config("report output cannot replace reserved project input"));
        }
        if !relative.starts_with("build")
            && let Ok(content) = fs::read_to_string(crate::configuration::tracked_input(
                project,
                Path::new(".infra/project.toml"),
            )?)
            && let Ok(settings) = toml::from_str::<toml::Value>(&content)
        {
            for key in ["source_roots", "test_roots", "generated_roots", "third_party_roots"] {
                if let Some(roots) = settings.get(key).and_then(toml::Value::as_array) {
                    for root in roots.iter().filter_map(toml::Value::as_str) {
                        let declared = project.join(root);
                        if declared.exists() && resolved.starts_with(declared.canonicalize()?) {
                            return Err(config("report output cannot be placed in a governed source root"));
                        }
                    }
                }
            }
        }
    }
    if path.is_file() {
        let existing: Value = serde_json::from_slice(&fs::read(path)?)
            .map_err(|_| config("report output would overwrite an existing project input"))?;
        if !(existing["schema"] == 1 && existing["tool"].is_object() && existing["status"].is_string()
            || existing["version"] == "2.1.0" && existing["runs"].is_array())
        {
            return Err(config(
                "output may replace only an existing cpp-infra report or SARIF file",
            ));
        }
    }
    Ok(resolved)
}

pub(crate) fn reserved_project_input(relative: &Path) -> bool {
    let first = relative
        .components()
        .next()
        .and_then(|part| part.as_os_str().to_str())
        .unwrap_or("");
    [".infra", ".git", "conan", "cmake"].contains(&first)
        || first.to_ascii_uppercase().starts_with("README")
        || [
            "CMakeLists.txt",
            "CMakePresets.json",
            "CMakeUserPresets.json",
            "conanfile.py",
            "conanfile.txt",
        ]
        .iter()
        .any(|reserved| relative == Path::new(reserved))
}

pub(crate) fn validate_sarif_destination(project: &Path, path: &Path) -> Result<PathBuf, Error> {
    let resolved = validate_report_destination(project, path)?;
    if let Ok(relative) = resolved.strip_prefix(project)
        && !relative.starts_with("build")
        && !relative.starts_with("target")
    {
        return Err(config("project-local SARIF output must be a build or target artifact"));
    }
    Ok(resolved)
}

impl Run {
    pub fn new(project: &Path, requested_report: Option<&Path>, context: Option<&Path>) -> Result<Self, Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(config)?
            .as_millis();
        let id = format!(
            "{now}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut value = json!({"schema":1,"tool":version(),"bundle_id":"development","run_id":id,"task_id":"analyze","project":project,"source_revision":null,"source_digest":null,"config_digest":null,"status":"error","started_at":OffsetDateTime::now_utc().format(&Rfc3339).map_err(config)?,"duration_ms":0,"external_versions":{},"commands":[],"diagnostics":[],"artifacts":[],"data":{"limitations":["Templates are analyzed only along instantiated paths."]}});
        let mut root = project.join("build/infra-runs").join(&id);
        let mut runtime_snapshot = None;
        let mut prepared_context = None;
        let mut source_excluded_paths = Vec::new();
        if let Some(path) = context {
            let ctx: Value = serde_json::from_slice(&fs::read(path)?).map_err(config)?;
            let ctx_project = Path::new(
                ctx["project"]
                    .as_str()
                    .ok_or_else(|| config("run-context project missing"))?,
            )
            .canonicalize()?;
            if ctx["schema"] != 1 || ctx_project != project {
                return Err(config("run-context schema/project mismatch"));
            }
            if let Some(paths) = ctx.get("source_excluded_paths") {
                source_excluded_paths = serde_json::from_value::<Vec<String>>(paths.clone())
                    .map_err(|error| config(format!("invalid source exclusions: {error}")))?;
            }
            for field in [
                "run_id",
                "task_id",
                "bundle_id",
                "config_digest",
                "source_digest",
                "profile",
                "report_root",
            ] {
                let raw = ctx[field]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| config(format!("run-context {field} missing")))?;
                if ["run_id", "task_id", "bundle_id", "config_digest", "source_digest"].contains(&field) {
                    value[field] = json!(raw);
                }
            }
            root = PathBuf::from(
                ctx["report_root"]
                    .as_str()
                    .ok_or_else(|| config("report_root missing"))?,
            );
            if !root.is_absolute()
                || !root.starts_with(project.join("build/infra-runs"))
                || root.components().any(|p| matches!(p, std::path::Component::ParentDir))
            {
                return Err(config("run-context report_root outside controlled run directory"));
            }
            value["data"]["profile"] = ctx["profile"].clone();
            if let Some(path) = ctx.get("build_context") {
                let path = PathBuf::from(
                    path.as_str()
                        .ok_or_else(|| config("build_context must be an absolute path"))?,
                );
                let canonical = path.canonicalize()?;
                if !path.is_absolute() || !canonical.starts_with(root.canonicalize()?) {
                    return Err(config("build_context is outside report_root"));
                }
                prepared_context = Some(canonical);
            }
            match (ctx.get("runtime_snapshot"), ctx.get("runtime_snapshot_digest")) {
                (None, None) => {}
                (Some(path), Some(expected)) => {
                    let path = PathBuf::from(path.as_str().ok_or_else(|| config("runtime_snapshot must be a path"))?);
                    let expected = expected
                        .as_str()
                        .ok_or_else(|| config("runtime_snapshot_digest missing"))?;
                    let canonical = path.canonicalize()?;
                    if !path.is_absolute() || !canonical.starts_with(root.canonicalize()?) {
                        return Err(config("runtime snapshot outside report_root"));
                    }
                    let content = fs::read(&canonical)?;
                    if digest::bytes(&content) != expected {
                        return Err(config("runtime snapshot SHA256 mismatch"));
                    }
                    let snapshot: Value = serde_json::from_slice(&content).map_err(config)?;
                    if snapshot["schema"] != 1 || snapshot["bundle_id"] != ctx["bundle_id"] {
                        return Err(config("runtime snapshot schema/bundle mismatch"));
                    }
                    let tools = snapshot["tools"]
                        .as_array()
                        .ok_or_else(|| config("runtime snapshot tools missing"))?;
                    let entries: Vec<_> = tools
                        .iter()
                        .filter(|tool| tool["name"] == "cpp-infra-analyze")
                        .collect();
                    if entries.len() != 1 {
                        return Err(config("runtime snapshot must identify analyze exactly once"));
                    }
                    let tool = entries[0];
                    let executable = std::env::current_exe()?.canonicalize()?;
                    let snapshot_binary = Path::new(
                        tool["binary"]
                            .as_str()
                            .ok_or_else(|| config("runtime snapshot binary missing"))?,
                    );
                    if !snapshot_binary.is_absolute() {
                        return Err(config("runtime snapshot binary must be an absolute path"));
                    }
                    let snapshot_binary_digest = digest::bytes(
                        &fs::read(snapshot_binary)
                            .map_err(|error| config(format!("runtime snapshot binary cannot be read: {error}")))?,
                    );
                    let executable_digest = digest::bytes(&fs::read(executable)?);
                    if tool["version"] != version()["version"]
                        || tool["revision"] != version()["revision"]
                        || tool["protocol"] != 1
                        || tool["executable_sha256"] != snapshot_binary_digest
                        || tool["executable_sha256"] != executable_digest
                    {
                        return Err(config("runtime snapshot analyze identity/executable mismatch"));
                    }
                    runtime_snapshot = Some((canonical, expected.to_owned()));
                }
                _ => {
                    return Err(config("runtime snapshot path and digest must appear together"));
                }
            }
        }
        validate_owned_path(project, &root)?;
        fs::create_dir_all(&root)?;
        root = root.canonicalize()?;
        if !root.starts_with(project.join("build/infra-runs")) {
            return Err(config("report_root symlink escapes project"));
        }
        value["report_root"] = json!(root);
        let report_root = root.clone();
        root = report_root.join("analyze").join(&id);
        validate_owned_path(project, &root)?;
        fs::create_dir_all(&root)?;
        root = root.canonicalize()?;
        if !root.starts_with(&report_root) {
            return Err(config("analysis work directory escapes report_root"));
        }
        value["data"]["analysis_directory"] = json!(root.strip_prefix(&report_root).map_err(config)?);
        let report = match requested_report {
            Some(path) if path.is_absolute() => path.to_owned(),
            Some(path) => project.join(path),
            None => root.join("report.json"),
        };
        let report = validate_report_destination(project, &report)?;
        if let Ok(relative) = report.strip_prefix(project)
            && !relative.starts_with("build")
            && !relative.starts_with("target")
        {
            source_excluded_paths.push(
                relative
                    .to_str()
                    .ok_or_else(|| config("source exclusion path must be UTF-8"))?
                    .to_owned(),
            );
        }
        source_excluded_paths.sort();
        source_excluded_paths.dedup();
        let excluded = crate::source_exclusions::validate(project, &source_excluded_paths)?;
        value["source_excluded_paths"] = json!(source_excluded_paths);
        let source_digest = digest::project(project, false, &excluded)?;
        let config_digest = digest::project(project, true, &excluded)?;
        if context.is_some() && (value["source_digest"] != source_digest || value["config_digest"] != config_digest) {
            return Err(config("stale run-context: source/config digest mismatch"));
        }
        value["source_digest"] = json!(source_digest);
        value["config_digest"] = json!(config_digest);
        Ok(Self {
            root,
            report_root,
            report,
            value,
            started: Instant::now(),
            source_digest,
            config_digest,
            excluded,
            runtime_snapshot,
            prepared_context,
            source_excluded_paths,
            configuration_inputs: Vec::new(),
        })
    }

    pub fn command(&mut self, mut command: Value) {
        if let Some(log) = command["log"].as_str().map(PathBuf::from)
            && let Ok(relative) = log.strip_prefix(&self.report_root)
        {
            command["log"] = json!(relative);
        }
        self.value["commands"]
            .as_array_mut()
            .expect("report commands array")
            .push(command);
    }

    pub fn artifact(&mut self, kind: &str, path: &Path, required: bool) -> Result<(), Error> {
        let relative = path
            .strip_prefix(&self.report_root)
            .map_err(|_| config("artifact is outside report_root"))?;
        self.value["artifacts"]
            .as_array_mut()
            .expect("artifacts array")
            .push(json!({"kind":kind,"path":relative,"sha256":digest::bytes(&fs::read(path)?),"required":required}));
        Ok(())
    }

    pub fn finish(&mut self, result: &Result<(), Error>) -> Result<(), Error> {
        let project = Path::new(
            self.value["project"]
                .as_str()
                .ok_or_else(|| config("report project missing"))?,
        );
        let report = validate_report_destination(project, &self.report)?;
        if result.is_ok() {
            crate::configuration::validate_inputs(project, &self.configuration_inputs)?;
        }
        crate::source_exclusions::validate(project, &self.source_excluded_paths)?;
        self.value["duration_ms"] = json!(self.started.elapsed().as_millis());
        self.value["status"] = json!(match result {
            Ok(()) => "passed",
            Err(Error::Gate(_)) => "failed",
            Err(Error::Cancelled) => "cancelled",
            Err(_) => "error",
        });
        if let Err(error) = result {
            self.value["data"]["error"] = json!(error.to_string());
        }
        atomic(&report, &serde_json::to_vec_pretty(&self.value).map_err(config)?)
    }
}

// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Validated checks, NOLINT policy, and bounded clang-tidy execution.
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::thread;

use serde_json::Value;
use serde_json::json;

use crate::clang_runtime::ClangRuntime;
use crate::compilation_database::TranslationUnit;
use crate::configuration::Configuration;
use crate::configuration::Policy;
use crate::configuration::Project;
use crate::configuration::inside;
use crate::diagnostics;
use crate::digest;
use crate::error::Error;
use crate::error::config;
use crate::process;
use crate::reporting::Run;
use crate::reporting::atomic;

fn matches(pattern: &str, value: &str) -> bool {
    let parts: Vec<_> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == value;
    }
    let Some(mut remaining) = value.strip_prefix(parts[0]) else {
        return false;
    };
    for part in &parts[1..parts.len() - 1] {
        let Some(found) = remaining.find(part) else {
            return false;
        };
        remaining = &remaining[found + part.len()..];
    }
    remaining.ends_with(parts[parts.len() - 1])
}

pub(crate) fn execute(
    project: &Path,
    roots: &Project,
    settings: &Configuration,
    policy: &Policy,
    units: &[TranslationUnit],
    inputs: &[PathBuf],
    run: &mut Run,
) -> Result<Vec<Value>, Error> {
    let tidy = process::discover("clang-tidy", false)?;
    let version = process::run(
        &tidy,
        &["--version".into()],
        project,
        &run.root.join("clang-tidy-version.log"),
        10,
    )?;
    run.command(version.record);
    if version.cancelled {
        return Err(Error::Cancelled);
    }
    if version.code != Some(0) || !version.output.lines().any(|line| line.contains("version 18.")) {
        return Err(config("supported clang-tidy version is 18.x"));
    }
    run.value["external_versions"]["clang-tidy"] = json!(version.output.trim());
    let runtime = ClangRuntime::load()?;
    let runtime_info = runtime.info();
    run.value["external_versions"]["libclang"] = runtime_info.clone();
    drop(runtime);
    let policy_path = crate::configuration::tracked_input(project, Path::new(".infra/analyze/policy/clang-tidy.yaml"))?;
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&fs::read(&policy_path)?).map_err(config)?;
    if !yaml.is_mapping() {
        return Err(config("clang-tidy policy must be a YAML mapping"));
    }
    let checks = policy
        .mandatory_checks
        .iter()
        .chain(&settings.additive_checks)
        .cloned()
        .collect::<Vec<_>>()
        .join(",");
    let mut common = vec![
        format!("--config-file={}", policy_path.display()),
        format!("--checks={checks}"),
        "--header-filter=.*".into(),
        "--extra-arg=-Xclang".into(),
        "--extra-arg=-analyzer-opt-analyze-headers".into(),
    ];
    let mut list_args = common.clone();
    list_args.push("--list-checks".into());
    let list = process::run(&tidy, &list_args, project, &run.root.join("clang-tidy-checks.log"), 15)?;
    run.command(list.record);
    if list.cancelled {
        return Err(Error::Cancelled);
    }
    if list.code != Some(0) || list.output.contains("Error:") || list.output.contains("error:") {
        return Err(config(format!("invalid clang-tidy policy: {}", list.output)));
    }
    let available: Vec<_> = list
        .output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "Enabled checks:")
        .collect();
    for pattern in policy.mandatory_checks.iter().chain(&settings.additive_checks) {
        if !available.iter().any(|check| matches(pattern, check)) {
            return Err(config(format!("unknown or unavailable clang-tidy check: {pattern}")));
        }
    }
    let mut dump_args = common.clone();
    dump_args.push("--dump-config".into());
    let dump = process::run(
        &tidy,
        &dump_args,
        project,
        &run.root.join("clang-tidy-effective-config.log"),
        15,
    )?;
    run.command(dump.record);
    if dump.cancelled {
        return Err(Error::Cancelled);
    }
    if dump.code != Some(0) || dump.timed_out {
        return Err(config("cannot resolve effective clang-tidy checks"));
    }
    let effective: serde_yaml::Value = serde_yaml::from_str(&dump.output)
        .map_err(|error| config(format!("invalid effective clang-tidy config: {error}")))?;
    let effective_checks = effective["Checks"]
        .as_str()
        .ok_or_else(|| config("effective clang-tidy Checks missing"))?;
    let mut pragma_selected = false;
    for pattern in effective_checks.split(',').map(str::trim) {
        let (enable, glob) = match pattern.strip_prefix('-') {
            Some(glob) => (false, glob),
            None => (true, pattern),
        };
        if matches(glob, "clang-diagnostic-#pragma-messages") {
            pragma_selected = enable;
        }
    }
    // Informational pragma warnings otherwise enter NOLINT statistics before
    // check filtering. Disable their emission only when the effective policy
    // already excludes that diagnostic; explicitly selected diagnostics remain.
    if !pragma_selected {
        common.push("--extra-arg=-Wno-#pragma-messages".into());
    }
    let mut directives = Vec::new();
    let mut overlays = Vec::new();
    let mut candidates = BTreeSet::new();
    candidates.extend(inputs.iter().cloned());
    for root in roots.source_roots.iter().chain(if settings.include_tests {
        roots.test_roots.iter()
    } else {
        [].iter()
    }) {
        for file in digest::files(project, &inside(project, root)?)? {
            if !crate::source_text::is_code(&file) {
                continue;
            }
            candidates.insert(file);
        }
    }
    for file in candidates {
        let content = crate::source_text::read(&file)?;
        if !content.contains("NOLINT") {
            continue;
        }
        let owned = crate::compilation_database::governed(project, &file, roots, settings.include_tests)?;
        let log = run.root.join(format!(
            "native-lexer-{}.log",
            run.value["commands"].as_array().map_or(0, Vec::len)
        ));
        let args = vec!["__lex".into(), "--input".into(), file.display().to_string()];
        let outcome = process::run(
            &std::env::current_exe()?,
            &args,
            project,
            &log,
            settings.timeout_per_translation_unit_seconds,
        )?;
        run.command(outcome.record);
        run.artifact("clang-lexer-log", &log, true)?;
        if outcome.cancelled {
            return Err(Error::Cancelled);
        }
        if outcome.timed_out {
            return Err(Error::Infrastructure(format!(
                "native lexical classification timed out: {}",
                file.display()
            )));
        }
        if outcome.code != Some(0) {
            return Err(if outcome.code == Some(2) {
                config(outcome.output)
            } else {
                Error::Infrastructure(format!(
                    "native lexical classification failed for {}: {}",
                    file.display(),
                    outcome.output
                ))
            });
        }
        let classified: Value = serde_json::from_str(&outcome.output)
            .map_err(|error| Error::Infrastructure(format!("invalid native lexer output: {error}")))?;
        if classified["schema"] != 1
            || classified["source_sha256"] != digest::bytes(content.as_bytes())
            || classified["libclang"]["version"] != runtime_info["version"]
        {
            return Err(config(format!(
                "native lexical input/runtime changed: {}",
                file.display()
            )));
        }
        let positions: Vec<[usize; 6]> = serde_json::from_value(classified["directives"].clone())
            .map_err(|error| Error::Infrastructure(format!("invalid native directive ranges: {error}")))?;
        for span in &positions {
            if !span.windows(2).all(|pair| pair[0] < pair[1])
                || !span
                    .iter()
                    .zip(b"NOLINT")
                    .all(|(offset, byte)| content.as_bytes().get(*offset) == Some(byte))
            {
                return Err(Error::Infrastructure(
                    "native lexer returned invalid physical NOLINT offsets".into(),
                ));
            }
        }
        if positions.is_empty() {
            continue;
        }
        for offsets in &positions {
            let offset = offsets[0];
            let prefix = &content[..offset];
            let line = prefix.bytes().filter(|&c| c == b'\n').count() + 1;
            let column = prefix.rfind('\n').map_or(offset + 1, |last| offset - last);
            let text = content.lines().nth(line - 1).unwrap_or("");
            directives.push(json!({"code":"analyze.nolint","rule":"analyze.nolint","severity":if policy.reject_nolint && owned {"error"} else {"note"},"message":if owned {"Source NOLINT directive is not an organizational exception"} else {"Dependency NOLINT comment was neutralized before diagnostic collection"},"project_relative_path":file.strip_prefix(project).ok(),"line":line,"column":column,"suppression_reason":null,"suppression_scope":text.trim(),"suppression_offsets":offsets,"owned":owned}));
        }
        if !policy.reject_nolint || !owned {
            let copy = run.root.join(format!("nolint-overlay-{}", overlays.len()));
            let mut sanitized = content.into_bytes();
            for offsets in positions {
                for offset in offsets {
                    sanitized[offset] = b' ';
                }
            }
            fs::write(&copy, sanitized)?;
            overlays.push(json!({"type":"file","name":file,"external-contents":copy,"use-external-name":false}));
        }
    }
    if directives.iter().any(|directive| directive["severity"] == "error") {
        return Ok(directives);
    }
    let overlay: Option<PathBuf> = if overlays.is_empty() {
        None
    } else {
        let path = run.root.join("nolint-overlay.yaml");
        atomic(
            &path,
            &serde_json::to_vec_pretty(&json!({"version":0,"use-external-names":false,"roots":overlays}))
                .map_err(config)?,
        )?;
        Some(path)
    };
    let mut all = directives;
    for batch in units.chunks(settings.jobs) {
        let outcomes = thread::scope(|scope| {
            let mut handles = Vec::new();
            for unit in batch {
                let mut args = common.clone();
                args.extend([
                    "-p".into(),
                    unit.database.display().to_string(),
                    format!("--export-fixes={}", unit.directory.join("diagnostics.yaml").display()),
                ]);
                if let Some(path) = &overlay {
                    args.push(format!("--vfsoverlay={}", path.display()));
                }
                args.push(unit.file.display().to_string());
                let tidy = &tidy;
                handles.push(scope.spawn(move || {
                    process::run(
                        tidy,
                        &args,
                        project,
                        &unit.directory.join("clang-tidy.log"),
                        settings.timeout_per_translation_unit_seconds,
                    )
                }));
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| Error::Infrastructure("analysis worker panicked".into()))?
                })
                .collect::<Result<Vec<_>, Error>>()
        })?;
        for (unit, outcome) in batch.iter().zip(outcomes) {
            let parsed = if outcome.cancelled {
                Err(Error::Cancelled)
            } else {
                diagnostics::parse(project, roots, settings.include_tests, unit, &outcome)
            };
            run.command(outcome.record);
            run.artifact("clang-tidy-log", &unit.directory.join("clang-tidy.log"), true)?;
            if unit.directory.join("diagnostics.yaml").is_file() {
                run.artifact(
                    "clang-tidy-diagnostics",
                    &unit.directory.join("diagnostics.yaml"),
                    false,
                )?;
            }
            all.extend(parsed?);
        }
    }
    Ok(all)
}

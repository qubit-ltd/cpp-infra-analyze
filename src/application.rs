// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! CLI adaptation, run coordination, and utility subprocess entry points.
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

use clap::Parser;
use serde_json::Value;
use serde_json::json;

use crate::clang_runtime;
use crate::clang_tidy;
use crate::cli_options::Action;
use crate::cli_options::CliOptions;
use crate::compilation_database;
use crate::configuration;
use crate::configuration::Configuration;
use crate::configuration::Exceptions;
use crate::configuration::Policy;
use crate::configuration::Project;
use crate::diagnostics;
use crate::digest;
use crate::error::Error;
use crate::error::config;
use crate::nolint;
use crate::preparation;
use crate::process;
use crate::reporting;
use crate::reporting::Run;
use crate::reporting::atomic;
use crate::sarif;
use crate::source_exclusions;
use crate::source_text;

/// Runs the CLI, preserving the shared error exit codes and writing a report on
/// failure.
///
/// This entry point parses process arguments, runs external build and analysis
/// processes, and writes only requested report files and build artifacts.
/// Cancellation terminates active subprocess groups. Errors are rendered to
/// stderr and returned as exit codes.
pub fn cli() -> i32 {
    let options = match CliOptions::try_parse() {
        Ok(options) => options,
        Err(error) => {
            let exit = if error.use_stderr() { 2 } else { 0 };
            let _ = error.print();
            return exit;
        }
    };
    if options.version {
        if options.json {
            println!("{}", reporting::version());
        } else {
            println!("cpp-infra-analyze {}", env!("CARGO_PKG_VERSION"));
        }
        return 0;
    }
    if let Some(action) = &options.command {
        let result: Option<Result<(), Error>> = match action {
            Action::Doctor { json: as_json } => Some((|| {
                let runtime = clang_runtime::ClangRuntime::load()?;
                if *as_json {
                    println!("{}", json!({"schema":1,"libclang":runtime.info()}));
                } else {
                    eprintln!("libclang: {}", runtime.info());
                }
                Ok(())
            })()),
            Action::Lex { input } => Some((|| {
                let source = source_text::read(input)?;
                let runtime = clang_runtime::ClangRuntime::load()?;
                let tokens = runtime.tokens(input, &source)?;
                let directives = nolint::positions(&source, &tokens)
                    .map_err(|error| config(format!("{}: {error}", input.display())))?;
                println!(
                    "{}",
                    json!({"schema":1,"source_sha256":digest::bytes(source.as_bytes()),"libclang":runtime.info(),"directives":directives})
                );
                Ok(())
            })()),
            _ => None,
        };
        if let Some(result) = result {
            return match result {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("cpp-infra-analyze: {error}");
                    error.exit_code()
                }
            };
        }
    }
    let _ = ctrlc::set_handler(|| process::CANCELLED.store(true, Ordering::SeqCst));
    match dispatch(&options) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("cpp-infra-analyze: {error}");
            error.exit_code()
        }
    }
}

fn dispatch(options: &CliOptions) -> Result<(), Error> {
    let project = options.project.canonicalize()?;
    if !project.is_dir() {
        return Err(config("--project must be a directory"));
    }
    let mut run = match Run::new(&project, options.report.as_deref(), options.run_context.as_deref()) {
        Ok(run) => run,
        Err(error) => {
            if let Some(path) = options.report.as_deref() {
                let path = if path.is_absolute() {
                    path.to_owned()
                } else {
                    project.join(path)
                };
                let path = reporting::validate_report_destination(&project, &path)?;
                let exclusions = match path.strip_prefix(&project) {
                    Ok(relative) if !relative.starts_with("build") && !relative.starts_with("target") => vec![
                        relative
                            .to_str()
                            .ok_or_else(|| config("source exclusion must be UTF-8"))?
                            .to_owned(),
                    ],
                    _ => Vec::new(),
                };
                source_exclusions::validate(&project, &exclusions)?;
                atomic(&path, &serde_json::to_vec_pretty(&json!({"schema":1,"tool":reporting::version(),"bundle_id":"development","run_id":null,"task_id":"analyze","project":project,"report_root":null,"source_revision":null,"source_digest":null,"config_digest":null,"source_excluded_paths":exclusions,"status":"error","started_at":null,"duration_ms":0,"external_versions":{},"commands":[],"diagnostics":[],"artifacts":[],"data":{"error":error.to_string()}})).map_err(config)?)?;
            }
            return Err(error);
        }
    };
    let result = perform(options, &project, &mut run);
    run.finish(&result)?;
    result
}

fn perform(options: &CliOptions, project: &Path, run: &mut Run) -> Result<(), Error> {
    run.configuration_inputs = [
        ".infra/project.toml",
        ".infra/analyze/config.toml",
        ".infra/analyze/policy/policy.toml",
        ".infra/analyze/policy/clang-tidy.yaml",
    ]
    .iter()
    .map(std::path::PathBuf::from)
    .collect();
    configuration::validate_inputs(project, &run.configuration_inputs)?;
    let settings: Configuration = configuration::load_input(project, Path::new(".infra/analyze/config.toml"))?;
    let roots: Project = configuration::load_input(project, Path::new(".infra/project.toml"))?;
    let policy: Policy = configuration::load_input(project, Path::new(".infra/analyze/policy/policy.toml"))?;
    configuration::validate(project, &settings, &roots, &policy)?;
    let action = options
        .command
        .as_ref()
        .ok_or_else(|| config("a plan or check command is required"))?;
    let profile = match action {
        Action::Plan { profile } | Action::Check { profile, .. } => profile.as_deref().unwrap_or(&settings.profile),
        _ => return Err(config("utility action reached project analysis")),
    };
    let declaration = roots
        .profiles
        .get(profile)
        .ok_or_else(|| config(format!("unknown project profile: {profile}")))?;
    if run.value["data"]["profile"]
        .as_str()
        .is_some_and(|expected| expected != profile)
    {
        return Err(config("run-context profile mismatch"));
    }
    run.value["data"]["profile"] = json!(profile);
    let exceptions = match &settings.exceptions {
        Some(path) => {
            run.configuration_inputs.push(path.clone());
            configuration::load_input::<Exceptions>(project, path)?
        }
        None => Exceptions {
            schema: 1,
            exceptions: Vec::new(),
        },
    };
    diagnostics::validate_exceptions(&exceptions, &policy)?;
    let sarif_output = match action {
        Action::Check { sarif: Some(path), .. } => {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                project.join(path)
            };
            Some(reporting::validate_sarif_destination(project, &absolute)?)
        }
        _ => None,
    };
    if matches!(action, Action::Plan { .. }) {
        let plan = json!({"schema":1,"tool":"cpp-infra-analyze","project":project,"profile":profile,"nodes":[{"id":"prepare","tool":"cpp-infra-verify","args":["--project",project,"prepare","--profile",profile,"--output","<runtime build-context.json>"],"profile":profile,"needs":[],"resource_locks":[],"timeout_seconds":900,"required":true,"output":"build-context"},{"id":"analyze","tool":"cpp-infra-analyze","kind":"discovery","args":["clang-tidy","-p","<runtime per-context database>"],"profile":profile,"needs":["prepare"],"resource_locks":["<runtime binary_dir>/.cpp-infra.lock"],"timeout_seconds":settings.timeout_per_translation_unit_seconds,"required":true,"output":"diagnostics"}]});
        println!("{}", serde_json::to_string_pretty(&plan).map_err(config)?);
        run.value["data"]["plan"] = plan;
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        return Err(config("analysis supports Linux Clang 18 + Ninja"));
    }
    if let Ok(git) = process::discover("git", false) {
        let revision = process::run(
            &git,
            &["rev-parse".into(), "--verify".into(), "HEAD".into()],
            project,
            &run.root.join("source-revision.log"),
            10,
        )?;
        run.command(revision.record);
        if revision.cancelled {
            return Err(Error::Cancelled);
        }
        let value = revision.output.trim();
        if revision.code == Some(0) && matches!(value.len(), 40 | 64) && value.bytes().all(|c| c.is_ascii_hexdigit()) {
            run.value["source_revision"] = json!(value);
        }
    }
    let (context, _lock) = preparation::prepare(project, profile, declaration, run, options.offline)?;
    let (units, inputs) = compilation_database::select(project, &context, &settings, &roots, run)?;
    let all = clang_tidy::execute(project, &roots, &settings, &policy, &units, &inputs, run)?;
    let diagnostics = diagnostics::aggregate(all, &exceptions);
    run.value["diagnostics"] = json!(diagnostics);
    let json_path = run.root.join("diagnostics.json");
    atomic(&json_path, &serde_json::to_vec_pretty(&diagnostics).map_err(config)?)?;
    run.artifact("analysis-diagnostics", &json_path, true)?;
    if let Some(requested) = sarif_output {
        let requested = reporting::validate_sarif_destination(project, &requested)?;
        let path = run.root.join("analysis.sarif");
        sarif::write(&path, &diagnostics)?;
        run.artifact("sarif", &path, true)?;
        let requested = reporting::validate_sarif_destination(project, &requested)?;
        if requested != path {
            atomic(&requested, &fs::read(&path)?)?;
        }
    }
    preparation::validate_consumption_state(project, &context, run)?;
    let failed = diagnostics.iter().any(|diagnostic: &Value| {
        ["warning", "error"].contains(&diagnostic["severity"].as_str().unwrap_or("error"))
            && diagnostic["suppression_reason"].is_null()
    });
    if failed {
        Err(Error::Gate("analysis diagnostics failed the quality gate".into()))
    } else {
        Ok(())
    }
}

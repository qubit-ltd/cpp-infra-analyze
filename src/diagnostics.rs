// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Structured clang-tidy diagnostics, precise exceptions, and context-aware
//! aggregation.
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::compilation_database::TranslationUnit;
use crate::compilation_database::governed;
use crate::configuration::Exceptions;
use crate::configuration::Policy;
use crate::configuration::Project;
use crate::digest;
use crate::error::Error;
use crate::error::config;
use crate::process::Outcome;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Export {
    #[serde(default)]
    diagnostics: Vec<Diagnostic>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Diagnostic {
    diagnostic_name: String,
    diagnostic_message: Message,
    level: String,
    #[serde(default)]
    notes: Vec<Message>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Message {
    message: String,
    #[serde(default)]
    file_path: PathBuf,
    #[serde(default)]
    file_offset: usize,
}

type Location = (Option<String>, Option<usize>, Option<usize>);

fn location(project: &Path, message: &Message) -> Result<Location, Error> {
    if message.file_path.as_os_str().is_empty() {
        return Ok((None, None, None));
    }
    let path = if message.file_path.is_absolute() {
        message.file_path.clone()
    } else {
        project.join(&message.file_path)
    }
    .canonicalize()?;
    let content = fs::read(&path)?;
    let offset = message.file_offset.min(content.len());
    let prefix = &content[..offset];
    let line = prefix.iter().filter(|&&c| c == b'\n').count() + 1;
    let column = prefix
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(offset + 1, |last| offset - last);
    let relative = path
        .strip_prefix(project)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"));
    Ok((relative, Some(line), Some(column)))
}

pub(crate) fn parse(
    project: &Path,
    roots: &Project,
    include_tests: bool,
    unit: &TranslationUnit,
    output: &Outcome,
) -> Result<Vec<Value>, Error> {
    if output.timed_out {
        return Ok(vec![
            json!({"code":"analyze.timeout","severity":"error","message":"clang-tidy translation unit timed out","project_relative_path":unit.file.strip_prefix(project).ok(),"rule":"analyze.timeout","origin_contexts":[{"target":unit.target,"fingerprint":unit.fingerprint}],"suppression_reason":null}),
        ]);
    }
    if output.code.is_none() {
        return Err(Error::Infrastructure(format!(
            "clang-tidy crashed; see {}",
            unit.directory.display()
        )));
    }
    if output
        .output
        .lines()
        .any(|line| line.trim_start().starts_with("Suppressed ") && line.contains(" NOLINT"))
    {
        return Err(config(format!(
            "cannot certify complete mandatory diagnostics for {}: clang-tidy still suppressed diagnostics using raw-line NOLINT; ownership cannot be inferred from suppression statistics (see clang-tidy.log)",
            unit.file.display()
        )));
    }
    let export = unit.directory.join("diagnostics.yaml");
    let diagnostics: Vec<Diagnostic> = if export.is_file() {
        serde_yaml::from_slice::<Export>(&fs::read(export)?)
            .map_err(|e| Error::Infrastructure(format!("unsupported clang-tidy 18 export: {e}")))?
            .diagnostics
    } else {
        Vec::new()
    };
    if diagnostics.is_empty()
        && (output.code != Some(0)
            || output.output.lines().any(|line| {
                line.contains("error:")
                    || line.contains("warning:")
                    || line.contains("LLVM ERROR")
                    || line.contains("PLEASE submit")
            }))
    {
        return Err(Error::Infrastructure(format!(
            "clang-tidy failed without parseable exported diagnostics: {}",
            output.output
        )));
    }
    let mut result = Vec::new();
    for diagnostic in diagnostics {
        let severity = match diagnostic.level.as_str() {
            "Warning" => "warning",
            "Error" | "Fatal" => "error",
            "Remark" | "Note" => "note",
            other => {
                return Err(Error::Infrastructure(format!(
                    "unknown clang-tidy diagnostic level {other}"
                )));
            }
        };
        let path = if diagnostic.diagnostic_message.file_path.is_absolute() {
            diagnostic.diagnostic_message.file_path.clone()
        } else {
            project.join(&diagnostic.diagnostic_message.file_path)
        }
        .canonicalize()
        .map_err(|e| Error::Infrastructure(format!("diagnostic path cannot be resolved: {e}")))?;
        if severity != "error" && !governed(project, &path, roots, include_tests)? {
            continue;
        }
        let (relative, line, column) = location(project, &diagnostic.diagnostic_message)?;
        let mut notes = Vec::new();
        for note in diagnostic.notes {
            let (relative, line, column) = location(project, &note)?;
            notes.push(json!({"message":note.message,"project_relative_path":relative,"line":line,"column":column}));
        }
        result.push(json!({"code":"analyze.diagnostic","severity":severity,"message":diagnostic.diagnostic_message.message,"project_relative_path":relative,"line":line,"column":column,"rule":diagnostic.diagnostic_name,"suppression_reason":null,"notes":notes,"macro_context":unit.macro_fingerprint,"origin_contexts":[{"target":unit.target,"fingerprint":unit.fingerprint,"header_fixture":unit.header}]}));
    }
    if output.code != Some(0) && !result.iter().any(|d| d["severity"] == "error") {
        result.push(json!({"code":"analyze.process_failure","severity":"error","message":"clang-tidy returned failure, including diagnostics outside governed roots","project_relative_path":null,"rule":"analyze.process_failure","suppression_reason":null}));
    }
    Ok(result)
}

pub(crate) fn validate_exceptions(exceptions: &Exceptions, policy: &Policy) -> Result<(), Error> {
    if exceptions.schema != 1 {
        return Err(config("unsupported exceptions schema"));
    }
    let mut unique = std::collections::BTreeSet::new();
    for exception in &exceptions.exceptions {
        if exception.reason.trim().is_empty()
            || !policy.exception_rules.contains(&exception.rule)
            || exception.rule.starts_with("analyze.")
            || exception.path.is_empty()
            || Path::new(&exception.path).is_absolute()
            || Path::new(&exception.path)
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
            || exception
                .message_digest
                .as_ref()
                .is_some_and(|s| s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(config(format!(
                "invalid or prohibited exception: {} {}",
                exception.rule, exception.path
            )));
        }
        if !unique.insert((&exception.rule, &exception.path, &exception.message_digest)) {
            return Err(config("duplicate exception"));
        }
    }
    Ok(())
}

pub(crate) fn aggregate(all: Vec<Value>, exceptions: &Exceptions) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for mut diagnostic in all {
        if diagnostic["severity"] == "warning" {
            for exception in &exceptions.exceptions {
                if diagnostic["rule"] == exception.rule
                    && diagnostic["project_relative_path"] == exception.path
                    && exception.message_digest.as_ref().is_none_or(|expected| {
                        diagnostic["message"]
                            .as_str()
                            .is_some_and(|s| digest::bytes(s.as_bytes()) == *expected)
                    })
                {
                    diagnostic["suppression_reason"] = json!(exception.reason);
                    break;
                }
            }
        }
        if let Some(existing) = merged.iter_mut().find(|existing| {
            [
                "project_relative_path",
                "rule",
                "line",
                "column",
                "message",
                "severity",
                "macro_context",
            ]
            .iter()
            .all(|field| existing[*field] == diagnostic[*field])
        }) {
            if let (Some(origins), Some(extra)) = (
                existing["origin_contexts"].as_array_mut(),
                diagnostic["origin_contexts"].as_array(),
            ) {
                origins.extend(extra.clone());
            }
        } else {
            merged.push(diagnostic);
        }
    }
    merged.sort_by_key(|d| {
        (
            d["project_relative_path"].to_string(),
            d["line"].as_u64(),
            d["rule"].to_string(),
            d["macro_context"].to_string(),
        )
    });
    merged
}

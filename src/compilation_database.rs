// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Compilation contexts selected from verify's File API target source lists.
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use crate::configuration::Configuration;
use crate::configuration::Project;
use crate::configuration::inside;
use crate::digest;
use crate::error::Error;
use crate::error::config;
use crate::preparation::BuildContext;
use crate::process;
use crate::reporting::Run;
use crate::reporting::atomic;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Entry {
    pub directory: PathBuf,
    pub file: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug)]
pub(crate) struct TranslationUnit {
    pub file: PathBuf,
    pub directory: PathBuf,
    pub database: PathBuf,
    pub target: String,
    pub fingerprint: String,
    pub macro_fingerprint: String,
    pub header: Option<PathBuf>,
}

impl Entry {
    pub fn argv(&self) -> Result<Vec<String>, Error> {
        match (&self.arguments, &self.command) {
            (Some(args), _) if !args.is_empty() => Ok(args.clone()),
            (None, Some(command)) => shell_words::split(command).map_err(config),
            _ => Err(config("compile entry must contain arguments or command")),
        }
    }

    fn source(&self) -> Result<PathBuf, Error> {
        let source = if self.file.is_absolute() {
            self.file.clone()
        } else {
            self.directory.join(&self.file)
        };
        source.canonicalize().map_err(Error::from)
    }

    fn base_args(&self) -> Result<Vec<String>, Error> {
        let source = self.source()?;
        let args = self.argv()?;
        let mut result = Vec::new();
        let mut skip = false;
        for arg in args {
            if skip {
                skip = false;
                continue;
            }
            if ["-o", "-MF", "-MT", "-MQ"].contains(&arg.as_str()) {
                skip = true;
                continue;
            }
            if ["-c", "-MD", "-MMD", "-MP"].contains(&arg.as_str()) {
                continue;
            }
            let path = if Path::new(&arg).is_absolute() {
                PathBuf::from(&arg)
            } else {
                self.directory.join(&arg)
            };
            if path.canonicalize().ok().as_ref() == Some(&source) {
                continue;
            }
            result.push(arg);
        }
        Ok(result)
    }
}

fn extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|p| p.to_str())
        .is_some_and(|p| extensions.contains(&p))
}

pub(crate) fn governed(project: &Path, path: &Path, roots: &Project, include_tests: bool) -> Result<bool, Error> {
    for root in roots.source_roots.iter().chain(if include_tests {
        roots.test_roots.iter()
    } else {
        [].iter()
    }) {
        if path.starts_with(inside(project, root)?) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn select(
    project: &Path,
    context: &BuildContext,
    settings: &Configuration,
    roots: &Project,
    run: &mut Run,
) -> Result<(Vec<TranslationUnit>, Vec<PathBuf>), Error> {
    let database = context
        .compile_commands
        .as_ref()
        .ok_or_else(|| config("missing compile_commands"))?;
    let entries: Vec<Entry> = serde_json::from_slice(&fs::read(database)?).map_err(config)?;
    if entries.is_empty() {
        return Err(config("empty compilation database"));
    }
    if entries.iter().any(|entry| {
        entry.file.starts_with(&context.binary_dir) && entry.file.components().any(|part| part.as_os_str() == "Unity")
    }) {
        return Err(config("Unity build compilation commands cannot be analyzed"));
    }
    let mut selected = Vec::new();
    let mut unique = BTreeSet::new();
    for name in &settings.targets {
        let target = context
            .targets
            .iter()
            .find(|target| &target.name == name)
            .ok_or_else(|| config(format!("target absent from File API: {name}")))?;
        let mut count = 0;
        for source in &target.sources {
            if !extension(source, &["cpp", "cc", "cxx", "c", "C", "m", "mm"]) {
                continue;
            }
            let source = source.canonicalize()?;
            if !governed(project, &source, roots, settings.include_tests)? {
                if roots
                    .test_roots
                    .iter()
                    .any(|root| source.starts_with(project.join(root)))
                    && !settings.include_tests
                {
                    continue;
                }
                if roots
                    .generated_roots
                    .iter()
                    .any(|root| source.starts_with(project.join(root)))
                {
                    continue;
                }
                return Err(config(format!(
                    "target implementation source is outside governed roots: {}",
                    source.display()
                )));
            }
            let mut matched = false;
            for entry in &entries {
                if entry.source()? != source {
                    continue;
                }
                let serialized = serde_json::to_string(entry).map_err(config)?;
                if serialized.contains("CMakeFiles/") && !serialized.contains(&format!("CMakeFiles/{name}.dir/")) {
                    continue;
                }
                let fingerprint = digest::bytes(serialized.as_bytes());
                let normalized = serde_json::to_string(&(entry.directory.clone(), entry.base_args()?))
                    .map_err(config)?
                    .replace(&project.display().to_string(), "$PROJECT");
                let macro_fingerprint = digest::bytes(normalized.as_bytes());
                matched = true;
                count += 1;
                if !unique.insert(fingerprint.clone()) {
                    continue;
                }
                selected.push((entry.clone(), name.clone(), fingerprint, macro_fingerprint, None));
            }
            if !matched {
                return Err(config(format!(
                    "selected target source has no compilation command: {}",
                    source.display()
                )));
            }
        }
        if count == 0 {
            return Err(config(format!(
                "selected target has no analyzable translation units: {name}"
            )));
        }
    }
    if selected.is_empty() {
        return Err(config("selected targets have no translation units"));
    }
    let mut included = BTreeSet::new();
    for (index, (entry, _, _, _, _)) in selected.iter().enumerate() {
        included.extend(discover_dependencies(entry, index, settings, run)?);
    }
    let mut headers = Vec::new();
    for root in &roots.source_roots {
        for file in digest::files(project, &inside(project, root)?)? {
            if extension(&file, &["h", "hh", "hpp", "hxx"]) && !included.contains(&file) {
                headers.push(file);
            }
        }
    }
    headers.sort();
    let original_count = selected.len();
    let mut header_count = 0;
    for header in headers {
        let contexts = selected.clone();
        let mut seen = BTreeSet::new();
        for (entry, target, _, macro_fingerprint, _) in contexts {
            if !seen.insert((target.clone(), macro_fingerprint.clone())) {
                continue;
            }
            let fixture = run.root.join(format!("header-fixture-{header_count}.cpp"));
            let name = header.to_str().ok_or_else(|| config("non UTF-8 header path"))?;
            if name.contains(['"', '\n', '\r']) {
                return Err(config("header path cannot be represented in an include fixture"));
            }
            fs::write(&fixture, format!("#include \"{name}\"\n"))?;
            let mut arguments = entry.base_args()?;
            arguments.extend(["-x".into(), "c++".into(), "-c".into(), fixture.display().to_string()]);
            let entry = Entry {
                directory: entry.directory,
                file: fixture,
                arguments: Some(arguments),
                command: None,
                output: None,
            };
            let fingerprint = digest::bytes(serde_json::to_string(&entry).map_err(config)?.as_bytes());
            selected.push((entry, target, fingerprint, macro_fingerprint, Some(header.clone())));
            header_count += 1;
        }
    }
    for (index, (entry, _, _, _, _)) in selected.iter().enumerate().skip(original_count) {
        included.extend(discover_dependencies(entry, index, settings, run)?);
    }
    run.value["data"]["header_fixtures"] = json!(header_count);
    run.value["data"]["header_fixture_source"] = json!(
        "Selected File API target compile command requirements; compiler dependency closure determines uncovered headers."
    );
    let mut result = Vec::new();
    for (index, (entry, target, fingerprint, macro_fingerprint, header)) in selected.into_iter().enumerate() {
        let directory = run.root.join(format!("tu-{index}"));
        fs::create_dir_all(&directory)?;
        atomic(
            &directory.join("compile_commands.json"),
            &serde_json::to_vec_pretty(&vec![&entry]).map_err(config)?,
        )?;
        result.push(TranslationUnit {
            file: entry.source()?,
            database: directory.clone(),
            directory,
            target,
            fingerprint,
            macro_fingerprint,
            header,
        });
    }
    run.value["data"]["translation_units"] = json!(result.len());
    Ok((result, included.into_iter().collect()))
}

fn discover_dependencies(
    entry: &Entry,
    index: usize,
    settings: &Configuration,
    run: &mut Run,
) -> Result<Vec<PathBuf>, Error> {
    let mut inputs = Vec::new();
    let mut args = entry.base_args()?;
    let compiler = PathBuf::from(args.remove(0));
    let depfile = run.root.join(format!("dependencies-{index}.d"));
    args.extend([
        "-M".into(),
        "-MF".into(),
        depfile.display().to_string(),
        "-MT".into(),
        "infra".into(),
        entry.source()?.display().to_string(),
    ]);
    let result = process::run(
        &compiler,
        &args,
        &entry.directory,
        &run.root.join(format!("dependencies-{index}.log")),
        settings.timeout_per_translation_unit_seconds,
    )?;
    run.command(result.record);
    if result.cancelled {
        return Err(Error::Cancelled);
    }
    if result.timed_out || result.code != Some(0) {
        return Err(Error::Gate(format!(
            "dependency discovery failed; generated headers must exist: {}",
            result.output
        )));
    }
    let text = fs::read_to_string(depfile)?.replace("\\\n", "");
    let (_, dependencies) = text
        .split_once(':')
        .ok_or_else(|| config("invalid compiler dependency output"))?;
    for path in shell_words::split(dependencies).map_err(config)? {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            entry.directory.join(path)
        };
        let path = path.canonicalize()?;
        crate::source_text::read(&path)?;
        inputs.push(path);
    }
    Ok(inputs)
}

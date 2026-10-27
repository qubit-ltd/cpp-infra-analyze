// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Stable cross-tool project input identity.
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use sha2::Digest;
use sha2::Sha256;

use crate::error::Error;
use crate::error::config;

pub(crate) fn bytes(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

pub(crate) fn files(project: &Path, root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let entry_path = entry.path();
        let relative = entry_path.strip_prefix(project).map_err(config)?;
        if relative.components().next().is_some_and(|part| {
            [".git", ".infra", "build", "target"]
                .iter()
                .any(|name| part.as_os_str() == *name)
        }) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            result.extend(files(project, &entry.path())?);
        } else if kind.is_file() {
            result.push(entry.path());
        }
    }
    Ok(result)
}

pub(crate) fn project(root: &Path, config_only: bool, excluded: &[PathBuf]) -> Result<String, Error> {
    let mut inputs = Vec::new();
    collect(root, root, config_only, excluded, &mut inputs)?;
    inputs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut digest = Sha256::new();
    for (name, path) in inputs {
        let content = fs::read(path)?;
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((content.len() as u64).to_le_bytes());
        digest.update(content);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn collect(
    root: &Path,
    directory: &Path,
    config_only: bool,
    excluded: &[PathBuf],
    output: &mut Vec<(String, PathBuf)>,
) -> Result<(), Error> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if !config_only && excluded.contains(&path) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        let relative = path.strip_prefix(root).map_err(config)?;
        let parts: Vec<_> = relative.components().map(|p| p.as_os_str().to_string_lossy()).collect();
        if matches!(parts[0].as_ref(), ".git" | "build" | "target") {
            continue;
        }
        if !config_only && parts[0] == ".infra" {
            continue;
        }
        if config_only
            && parts[0] == ".infra"
            && parts
                .get(1)
                .is_some_and(|p| matches!(p.as_ref(), ".tmp" | "transactions" | ".transactions"))
        {
            continue;
        }
        if kind.is_dir() {
            collect(root, &path, config_only, excluded, output)?;
        } else if kind.is_file()
            && (!config_only
                || parts[0] == ".infra"
                || parts[0] == "conan"
                || ["CMakeLists.txt", "CMakePresets.json", "conanfile.py", "conanfile.txt"]
                    .contains(&relative.to_str().ok_or_else(|| config("non UTF-8 input path"))?))
        {
            output.push((
                relative
                    .to_str()
                    .ok_or_else(|| config("non UTF-8 input path"))?
                    .replace('\\', "/"),
                path,
            ));
        }
    }
    Ok(())
}

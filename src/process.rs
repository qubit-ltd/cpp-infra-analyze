// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Logged subprocesses with process-tree timeout and cancellation.
use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

use crate::error::Error;

pub(crate) static CANCELLED: AtomicBool = AtomicBool::new(false);

pub(crate) struct Outcome {
    pub record: Value,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub output: String,
}

pub(crate) fn run(program: &Path, args: &[String], cwd: &Path, log: &Path, timeout: u64) -> Result<Outcome, Error> {
    if CANCELLED.load(Ordering::SeqCst) {
        return Err(Error::Cancelled);
    }
    let file = File::create(log)?;
    let started = Instant::now();
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file))
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| Error::Infrastructure(format!("cannot spawn {}: {e}", program.display())))?;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if CANCELLED.load(Ordering::SeqCst) || started.elapsed() >= Duration::from_secs(timeout) {
            timed_out = !CANCELLED.load(Ordering::SeqCst);
            // The child starts its own process group, so descendants cannot survive
            // cancellation.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let output = fs::read_to_string(log)?;
    let env: BTreeMap<&str, String> = ["CPP_INFRA_BIN_DIR", "PATH", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (*name, value)))
        .collect();
    let cancelled = CANCELLED.load(Ordering::SeqCst);
    let record = json!({"program":program,"args":args,"cwd":cwd,"env":env,"exit_code":status.code(),"duration_ms":started.elapsed().as_millis(),"log":log,"timed_out":timed_out,"cancelled":cancelled});
    Ok(Outcome {
        record,
        code: status.code(),
        timed_out,
        cancelled,
        output,
    })
}

pub(crate) fn discover(binary: &str, bundled: bool) -> Result<std::path::PathBuf, Error> {
    if bundled && let Some(directory) = std::env::var_os("CPP_INFRA_BIN_DIR") {
        let path = std::path::PathBuf::from(directory).join(binary);
        if !path.is_file() {
            return Err(Error::Infrastructure(format!(
                "bundle binary missing: {}",
                path.display()
            )));
        }
        return Ok(path);
    }
    let candidates: Vec<String> = if binary == "clang-tidy" {
        vec!["clang-tidy-18".into(), "clang-tidy".into()]
    } else {
        vec![binary.into()]
    };
    for candidate in candidates {
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let path = directory.join(&candidate);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(Error::Infrastructure(format!("required program not found: {binary}")))
}

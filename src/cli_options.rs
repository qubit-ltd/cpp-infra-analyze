// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! User-facing argument parsing.
use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

#[derive(Debug, Parser)]
#[command(
    name = "cpp-infra-analyze",
    disable_version_flag = true,
    about = "Governed clang-tidy analysis using a verified build context"
)]
pub(crate) struct CliOptions {
    #[arg(long, global = true, default_value = ".")]
    pub project: PathBuf,
    #[arg(long, global = true)]
    pub report: Option<PathBuf>,
    #[arg(long, global = true)]
    pub offline: bool,
    #[arg(long, global = true)]
    pub run_context: Option<PathBuf>,
    #[arg(long)]
    pub version: bool,
    #[arg(long)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<Action>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// Check the required native LLVM runtime without building a project.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    #[command(name = "__lex", hide = true)]
    Lex {
        #[arg(long)]
        input: PathBuf,
    },
    /// Show preparation and runtime discovery without starting processes.
    Plan {
        #[arg(long)]
        profile: Option<String>,
    },
    /// Build selected targets and analyze every selected compilation context.
    Check {
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        sarif: Option<PathBuf>,
    },
}

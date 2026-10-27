// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use crate::compilation_database;
use crate::configuration::Configuration;
use crate::configuration::Project;
use crate::preparation::BuildContext;
use crate::reporting::Run;

#[test]
fn test_selection_rejects_missing_database_and_unrepresentable_public_header() {
    let temp = tempfile::tempdir().expect("temporary project");
    let project = temp.path().canonicalize().expect("project");
    fs::create_dir(project.join("src")).expect("source directory");
    let source = project.join("src/main.cpp");
    fs::write(&source, "int clean(){return 42;}\n").expect("source");
    let binary = project.join("build/infra/analysis");
    fs::create_dir_all(&binary).expect("binary directory");
    let mut context: BuildContext = serde_json::from_value(json!({
        "schema":1,"project":project,"profile":"analysis","binary_dir":binary,
        "configure_preset":"analysis","build_preset":"analysis","source_digest":"source","config_digest":"config",
        "fingerprint":"fingerprint","configuration_inputs":[],"cmake_cache_digest":"cache",
        "compiler":{"id":"Clang","path":"/usr/bin/clang++-18","version":"18.1.3"},"generator":"Ninja",
        "compile_commands":null,"targets":[{"name":"example","type":"STATIC_LIBRARY","sources":[source],"artifacts":[]}]
    }))
    .expect("context");
    let settings: Configuration =
        toml::from_str("schema=1\nprofile='analysis'\ntargets=['example']").expect("settings");
    let roots: Project = toml::from_str(
        "schema=1\nkind='library'\nsource_roots=['src']\n[profiles.analysis]\nconfigure='analysis'\nbuild='analysis'",
    )
    .expect("roots");
    let mut run = Run::new(&project, None, None).expect("run");
    let error =
        compilation_database::select(&project, &context, &settings, &roots, &mut run).expect_err("missing database");
    assert!(error.to_string().contains("missing compile_commands"));
    let database = binary.join("compile_commands.json");
    fs::write(
        &database,
        serde_json::to_vec(
            &json!([{"directory":project,"file":source,"arguments":["/usr/bin/clang++-18","-c",source]}]),
        )
        .expect("database JSON"),
    )
    .expect("database");
    context.compile_commands = Some(database.clone());
    // Add the filename after the run digest: this directly exercises the selection
    // contract rather than the earlier project digest's independent UTF8 guard.
    fs::write(
        project.join("src").join(OsString::from_vec(b"bad-\xff.hpp".to_vec())),
        "inline int clean_header(){return 42;}\n",
    )
    .expect("non-UTF8 header");
    let error = compilation_database::select(&project, &context, &settings, &roots, &mut run)
        .expect_err("header cannot be encoded in derived TU");
    assert!(error.to_string().contains("non UTF-8 header path"));
    fs::write(&database, serde_json::to_vec(&json!([{"directory":project,"file":binary.join("CMakeFiles/Unity/unity.cpp"),"arguments":["/usr/bin/clang++-18"]}])).expect("unity JSON")).expect("unity database");
    let error =
        compilation_database::select(&project, &context, &settings, &roots, &mut run).expect_err("Unity database");
    assert!(error.to_string().contains("Unity build compilation commands"));
    let compiler = project.join("dependency-output-fixture.py");
    fs::write(&compiler, "#!/usr/bin/env python3\nimport pathlib,sys\npathlib.Path(sys.argv[sys.argv.index('-MF')+1]).write_text('malformed dependency output')\n").expect("controlled dependency fault");
    fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).expect("executable dependency fixture");
    fs::write(
        &database,
        serde_json::to_vec(&json!([{"directory":project,"file":source,"arguments":[compiler,"-c",source]}]))
            .expect("fault JSON"),
    )
    .expect("fault database");
    let error = compilation_database::select(&project, &context, &settings, &roots, &mut run)
        .expect_err("invalid dependency output");
    assert!(error.to_string().contains("invalid compiler dependency output"));
}

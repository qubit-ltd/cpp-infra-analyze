// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    project: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let temp = TempDir::new().expect("temporary fixture");
        let project = temp.path().join("project");
        let bin = temp.path().join("bin");
        for dir in ["src", "include", ".infra/analyze/policy"] {
            fs::create_dir_all(project.join(dir)).expect("fixture directories");
        }
        fs::create_dir_all(&bin).expect("bin directory");
        fs::write(project.join("src/main.cpp"), source).expect("source");
        fs::write(project.join("CMakeLists.txt"), "cmake_minimum_required(VERSION 3.25)\nproject(fixture LANGUAGES CXX)\nset(CMAKE_EXPORT_COMPILE_COMMANDS ON)\nadd_library(example STATIC src/main.cpp)\ntarget_include_directories(example PUBLIC include)\n").expect("cmake");
        fs::write(project.join(".infra/project.toml"), "schema = 1\nkind = 'library'\nsource_roots = ['src', 'include']\ntest_roots = []\ngenerated_roots = []\nthird_party_roots = []\n[profiles.analysis]\nconfigure = 'analysis'\nbuild = 'analysis'\n").expect("project config");
        fs::write(project.join(".infra/analyze/config.toml"), "schema = 1\nprofile = 'analysis'\ntargets = ['example']\ninclude_tests = false\njobs = 2\ntimeout_per_translation_unit_seconds = 30\n").expect("analysis config");
        fs::write(
            project.join(".infra/analyze/policy/clang-tidy.yaml"),
            "Checks: '-*,clang-analyzer-*,bugprone-*'\nWarningsAsErrors: ''\nHeaderFilterRegex: '.*'\n",
        )
        .expect("policy");
        fs::write(project.join(".infra/analyze/policy/policy.toml"), "schema = 1\nmandatory_checks = ['clang-analyzer-*', 'bugprone-*']\nexception_rules = ['bugprone-unused-return-value']\nreject_nolint = true\n").expect("policy metadata");
        let script = include_str!("fixtures/verify.py");
        fs::write(bin.join("cpp-infra-verify"), script).expect("verify fixture");
        fs::set_permissions(bin.join("cpp-infra-verify"), fs::Permissions::from_mode(0o755))
            .expect("verify executable");
        Self {
            _temp: temp,
            project,
            bin,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
            .arg("--project")
            .arg(&self.project)
            .env("CPP_INFRA_BIN_DIR", &self.bin)
            .args(args)
            .output()
            .expect("analysis invocation")
    }

    fn report(&self) -> Value {
        serde_json::from_slice(&fs::read(self.project.join("build/report.json")).expect("report exists"))
            .expect("valid report")
    }

    fn alter_context(&self, python: &str) {
        let path = self.bin.join("cpp-infra-verify");
        let script = fs::read_to_string(&path).expect("verify fixture script");
        let insertion = script.rfind("output.parent.mkdir").expect("final context output");
        fs::write(
            path,
            format!("{}{python}\n{}", &script[..insertion], &script[insertion..]),
        )
        .expect("modify controlled context");
    }

    fn install_fake_tidy(&self, body: &str) -> String {
        let path = self.bin.join("clang-tidy-18");
        let script = format!(
            "#!/usr/bin/env python3\nimport sys,time,os,signal,resource\nresource.setrlimit(resource.RLIMIT_CORE,(0,0))\nif '--version' in sys.argv:\n print('LLVM version 18.1.3');sys.exit(0)\nif '--dump-config' in sys.argv:\n print(\"Checks: '-*,clang-analyzer-*,bugprone-*'\");sys.exit(0)\nif '--list-checks' in sys.argv:\n print('Enabled checks:\\n clang-analyzer-core.NullDereference\\n bugprone-unused-return-value');sys.exit(0)\n{body}\n"
        );
        fs::write(&path, script).expect("controlled tidy");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable tidy");
        format!("{}:{}", self.bin.display(), std::env::var("PATH").expect("PATH"))
    }

    fn run_with_fake_tidy(&self, body: &str) -> Output {
        let search = self.install_fake_tidy(body);
        Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
            .arg("--project")
            .arg(&self.project)
            .args(["--report", "build/report.json", "check"])
            .env("CPP_INFRA_BIN_DIR", &self.bin)
            .env("PATH", search)
            .output()
            .expect("analysis process")
    }
}

#[test]
fn test_real_clang_tidy_detects_null_dereference_and_exports_relative_sarif() {
    let fixture = Fixture::new("int broken() { int *p = nullptr; return *p; }\n");
    let result = fixture.run(&[
        "--report",
        "build/report.json",
        "check",
        "--sarif",
        "build/analysis.sarif",
    ]);
    assert_eq!(
        result.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = fixture.report();
    assert_eq!(report["status"], "failed");
    assert!(
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["rule"] == "clang-analyzer-core.NullDereference")
    );
    let sarif: Value = serde_json::from_slice(&fs::read(fixture.project.join("build/analysis.sarif")).expect("SARIF"))
        .expect("SARIF JSON");
    let uri = sarif["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
        .as_str()
        .expect("relative URI");
    assert!(!Path::new(uri).is_absolute());
    assert_eq!(report["schema"], 1);
    assert_eq!(report["bundle_id"], "development");
    assert!(report["source_digest"].as_str().is_some_and(|s| s.len() == 64));
}

#[test]
fn test_clean_project_and_unused_public_header_are_actually_analyzed() {
    let fixture = Fixture::new("int clean() { return 42; }\n");
    fs::write(
        fixture.project.join("include/unused.hpp"),
        "inline int broken_header() { int *p = nullptr; return *p; }\n",
    )
    .expect("header");
    let result = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        result.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = fixture.report();
    assert!(
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["project_relative_path"] == "include/unused.hpp")
    );
    assert_eq!(report["data"]["header_fixtures"], 1);
}

#[test]
fn test_config_typo_rejected_before_preparation() {
    let fixture = Fixture::new("int clean() { return 42; }\n");
    let config = fixture.project.join(".infra/analyze/config.toml");
    fs::write(
        &config,
        format!("{}\njbos = 3\n", fs::read_to_string(&config).expect("config")),
    )
    .expect("typo");
    let result = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(fixture.report()["status"], "error");
    assert!(!fixture.project.join("build/infra/analysis/CMakeCache.txt").exists());
}

#[test]
fn test_plan_does_not_spawn_verify_or_clang_tidy() {
    let fixture = Fixture::new("int clean() { return 42; }\n");
    fs::write(
        fixture.bin.join("cpp-infra-verify"),
        "#!/usr/bin/env python3\nraise SystemExit(91)\n",
    )
    .expect("unusable verify");
    let result = fixture.run(&["plan", "--profile", "analysis"]);
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let plan: Value = serde_json::from_slice(&result.stdout).expect("plan JSON");
    assert_eq!(plan["nodes"][0]["tool"], "cpp-infra-verify");
    assert_eq!(plan["nodes"][1]["kind"], "discovery");
    assert!(!fixture.project.join("build/infra/analysis/CMakeCache.txt").exists());
}

#[test]
fn test_clean_check_reports_real_commands_and_build_context() {
    let fixture = Fixture::new("int clean() { return 42; }\n");
    let result = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = fixture.report();
    assert_eq!(report["status"], "passed");
    assert!(report["commands"].as_array().expect("commands").len() >= 3);
    assert_eq!(report["data"]["translation_units"], 1);
    assert!(
        report["external_versions"]["clang-tidy"]
            .as_str()
            .expect("version")
            .contains("18")
    );
}

#[test]
fn test_nolint_cannot_hide_mandatory_diagnostic() {
    let fixture = Fixture::new("int broken() { int *p = nullptr; return *p; } // NOLINT\n");
    let result = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(
        fixture.report()["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "analyze.nolint")
    );
}

#[test]
fn test_unknown_additive_check_is_configuration_error() {
    let fixture = Fixture::new("int clean() { return 42; }\n");
    let config = fixture.project.join(".infra/analyze/config.toml");
    fs::write(
        &config,
        format!(
            "{}\nadditive_checks = ['definitely-not-a-clang-check']\n",
            fs::read_to_string(&config).expect("config")
        ),
    )
    .expect("check config");
    let result = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        result.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn test_exact_warning_exception_preserves_suppressed_diagnostic() {
    let fixture = Fixture::new("#include <algorithm>\nint clean(){int a[2]={};std::remove(a,a+2,1);return 42;}\n");
    let config = fixture.project.join(".infra/analyze/config.toml");
    fs::write(
        &config,
        format!(
            "{}\nexceptions = '.infra/analyze/exceptions.toml'\n",
            fs::read_to_string(&config).expect("config")
        ),
    )
    .expect("exceptions config");
    fs::write(fixture.project.join(".infra/analyze/exceptions.toml"), "schema = 1\n[[exceptions]]\nrule = 'bugprone-unused-return-value'\npath = 'src/main.cpp'\nreason = 'Fixture deliberately ignores this return value'\n").expect("exceptions");
    let result = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        result.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        fixture.report()["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["suppression_reason"].as_str().is_some())
    );
}

#[test]
fn test_same_source_with_distinct_macros_keeps_both_contexts() {
    let fixture = Fixture::new(
        "#ifdef BROKEN\nint sample(){int *p=nullptr;return *p;}\n#else\nint sample(){return 42;}\n#endif\n",
    );
    let cmake = fixture.project.join("CMakeLists.txt");
    fs::write(
        &cmake,
        format!(
            "{}\nadd_library(other STATIC src/main.cpp)\ntarget_compile_definitions(other PRIVATE BROKEN=1)\n",
            fs::read_to_string(&cmake).expect("cmake")
        ),
    )
    .expect("second target");
    let config = fixture.project.join(".infra/analyze/config.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .expect("config")
            .replace("['example']", "['example', 'other']"),
    )
    .expect("targets");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = fixture.report();
    assert_eq!(report["data"]["translation_units"], 2);
    assert_eq!(report["diagnostics"][0]["origin_contexts"][0]["target"], "other");
}

#[test]
fn test_shared_header_diagnostics_merge_origins_for_equal_macro_contexts() {
    let fixture = Fixture::new("#include \"shared.hpp\"\nint sample(){return broken_header();}\n");
    fs::write(
        fixture.project.join("src/other.cpp"),
        "#include \"shared.hpp\"\nint other(){return broken_header();}\n",
    )
    .expect("other TU");
    fs::write(
        fixture.project.join("include/shared.hpp"),
        "inline int broken_header(){int*p=nullptr;return *p;}\n",
    )
    .expect("header");
    let cmake = fixture.project.join("CMakeLists.txt");
    fs::write(
        &cmake,
        fs::read_to_string(&cmake)
            .expect("cmake")
            .replace("STATIC src/main.cpp", "STATIC src/main.cpp src/other.cpp"),
    )
    .expect("two sources");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = fixture.report();
    let diagnostic = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| d["project_relative_path"] == "include/shared.hpp")
        .expect("header diagnostic");
    assert_eq!(diagnostic["origin_contexts"].as_array().expect("origins").len(), 2);
    assert_eq!(report["data"]["header_fixtures"], 0);
}

#[test]
fn test_empty_database_and_stale_context_are_rejected() {
    for (modification, expected) in [
        (
            "(build / 'compile_commands.json').write_text('[]')",
            "empty compilation database",
        ),
        ("context['source_digest'] = '0' * 64", "stale verify build-context"),
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        fixture.alter_context(modification);
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
}

#[test]
fn test_unity_generator_and_no_database_are_rejected() {
    for modification in [
        "context['generator'] = 'Ninja Multi-Config'",
        "context['compile_commands'] = None",
        "with open(build / 'CMakeCache.txt', 'a') as f: f.write('\\nCMAKE_UNITY_BUILD:BOOL=ON\\n')",
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        fixture.alter_context(modification);
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_third_party_warning_filtered_but_compile_failure_cannot_be_hidden() {
    let fixture = Fixture::new("#include \"../third_party/third.hpp\"\nint clean(){return 42;}\n");
    fs::create_dir_all(fixture.project.join("third_party")).expect("third-party directory");
    fs::write(
        fixture.project.join("third_party/third.hpp"),
        "inline int broken(){int*p=nullptr;return *p;}\n",
    )
    .expect("third party");
    let config = fixture.project.join(".infra/project.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .expect("project")
            .replace("third_party_roots = []", "third_party_roots = ['third_party']"),
    )
    .expect("third party root");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        fixture.report()
    );
    assert!(
        fixture.report()["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .is_empty()
    );
    fs::write(
        fixture.project.join("third_party/third.hpp"),
        "#error Missing third-party configuration\n",
    )
    .expect("invalid third party");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn test_timeout_crash_and_unparseable_failure_never_pass() {
    for (body, code, message) in [
        ("time.sleep(20)", 1, "analyze.timeout"),
        ("os.kill(os.getpid(),signal.SIGKILL)", 3, "crashed"),
        (
            "print('error: broken export protocol');sys.exit(1)",
            3,
            "without parseable",
        ),
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        let config = fixture.project.join(".infra/analyze/config.toml");
        fs::write(
            &config,
            fs::read_to_string(&config)
                .expect("config")
                .replace("seconds = 30", "seconds = 1"),
        )
        .expect("short timeout");
        let output = fixture.run_with_fake_tidy(body);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{body}: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            fixture.report()
        );
        assert!(
            serde_json::to_string(&fixture.report())
                .expect("report string")
                .contains(message)
        );
    }
}

#[test]
fn test_only_header_change_triggers_fresh_analysis() {
    let fixture = Fixture::new("#include \"value.hpp\"\nint clean(){return value();}\n");
    let header = fixture.project.join("include/value.hpp");
    fs::write(&header, "inline int value(){return 42;}\n").expect("clean header");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(0)
    );
    let before = fixture.report()["source_digest"].clone();
    fs::write(&header, "inline int value(){int*p=nullptr;return *p;}\n").expect("broken header");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(1)
    );
    assert_ne!(before, fixture.report()["source_digest"]);
}

#[test]
fn test_symlink_and_overlapping_roots_rejected() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let config = fixture.project.join(".infra/project.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .expect("project config")
            .replace("test_roots = []", "test_roots = ['src']"),
    )
    .expect("overlapping roots");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("overlapping"));
    fs::write(
        &config,
        fs::read_to_string(&config)
            .expect("project config")
            .replace("test_roots = ['src']", "test_roots = []")
            .replace("source_roots = ['src', 'include']", "source_roots = ['escape']"),
    )
    .expect("escape config");
    std::os::unix::fs::symlink(fixture._temp.path(), fixture.project.join("escape")).expect("symlink");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("escapes"));
}

#[test]
fn test_nolint_in_string_is_not_a_suppression_directive() {
    let fixture = Fixture::new("const char* label(){return \"NOLINT\";}\n");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_allowed_nolint_retains_scope_and_cannot_suppress_mandatory_rule() {
    let fixture = Fixture::new("int broken(){int*p=nullptr;return *p;} // NOLINT\n");
    let policy = fixture.project.join(".infra/analyze/policy/policy.toml");
    fs::write(
        &policy,
        fs::read_to_string(&policy)
            .expect("policy")
            .replace("reject_nolint = true", "reject_nolint = false"),
    )
    .expect("policy");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = fixture.report();
    let diagnostics = report["diagnostics"].as_array().expect("diagnostics");
    assert!(
        diagnostics
            .iter()
            .any(|d| d["code"] == "analyze.nolint" && d["suppression_scope"].is_string())
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| d["rule"] == "clang-analyzer-core.NullDereference")
    );
}

#[test]
fn test_runtime_snapshot_accepts_matching_identity_and_rejects_tampering() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(0)
    );
    let report = fixture.report();
    let root = PathBuf::from(report["report_root"].as_str().expect("root"));
    let binary = Path::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
        .canonicalize()
        .expect("binary");
    let mut snapshot = serde_json::json!({"schema":1,"bundle_id":"main-fixture","tools":[{"name":"cpp-infra-analyze","version":"0.1.0","revision":null,"protocol":1,"binary":binary,"executable_sha256":Sha256::digest(fs::read(&binary).expect("binary bytes")).iter().map(|byte| format!("{byte:02x}")).collect::<String>()}],"offline":true,"stale":false});
    let snapshot_path = root.join("runtime-snapshot.json");
    let context_path = root.join("test-context.json");
    let bytes = serde_json::to_vec(&snapshot).expect("snapshot JSON");
    fs::write(&snapshot_path, &bytes).expect("snapshot");
    let mut context = serde_json::json!({"schema":1,"run_id":report["run_id"],"task_id":"analyze-test","bundle_id":"main-fixture","project":fixture.project,"profile":"analysis","source_digest":report["source_digest"],"config_digest":report["config_digest"],"report_root":root,"runtime_snapshot":snapshot_path,"runtime_snapshot_digest":Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect::<String>()});
    fs::write(&context_path, serde_json::to_vec(&context).expect("context JSON")).expect("context");
    let output = fixture.run(&[
        "--report",
        "build/report.json",
        "--run-context",
        context_path.to_str().expect("path"),
        "check",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.report()["bundle_id"], "main-fixture");
    let child: Value = serde_json::from_slice(
        &fs::read(
            root.join(
                fixture.report()["data"]["analysis_directory"]
                    .as_str()
                    .expect("analysis directory"),
            )
            .join("verify-run-context.json"),
        )
        .expect("child context"),
    )
    .expect("child JSON");
    assert_eq!(child["runtime_snapshot_digest"], context["runtime_snapshot_digest"]);
    fs::write(&snapshot_path, b"tampered").expect("tamper");
    let output = fixture.run(&[
        "--report",
        "build/report.json",
        "--run-context",
        context_path.to_str().expect("path"),
        "check",
    ]);
    assert_eq!(output.status.code(), Some(2));
    snapshot["tools"][0]["version"] = serde_json::json!("99.0.0");
    let bytes = serde_json::to_vec(&snapshot).expect("bad snapshot JSON");
    fs::write(&snapshot_path, &bytes).expect("identity tamper");
    context["runtime_snapshot_digest"] = serde_json::json!(
        Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    fs::write(&context_path, serde_json::to_vec(&context).expect("context JSON")).expect("context");
    let output = fixture.run(&[
        "--report",
        "build/report.json",
        "--run-context",
        context_path.to_str().expect("path"),
        "check",
    ]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn test_generated_header_is_built_before_dependency_and_analysis_steps() {
    let fixture = Fixture::new("#include \"generated.hpp\"\nint clean(){return generated();}\n");
    fs::write(
        fixture.project.join("generated.hpp.in"),
        "inline int generated(){return 42;}\n",
    )
    .expect("generator input");
    let cmake = fixture.project.join("CMakeLists.txt");
    fs::write(&cmake, format!("{}\nadd_custom_command(OUTPUT ${{CMAKE_CURRENT_BINARY_DIR}}/generated.hpp COMMAND ${{CMAKE_COMMAND}} -E copy ${{CMAKE_CURRENT_SOURCE_DIR}}/generated.hpp.in ${{CMAKE_CURRENT_BINARY_DIR}}/generated.hpp DEPENDS generated.hpp.in)\nadd_custom_target(generate-header DEPENDS ${{CMAKE_CURRENT_BINARY_DIR}}/generated.hpp)\nadd_dependencies(example generate-header)\ntarget_include_directories(example PRIVATE ${{CMAKE_CURRENT_BINARY_DIR}})\n",fs::read_to_string(&cmake).expect("cmake"))).expect("generator command");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.project.join("build/infra/analysis/generated.hpp").is_file());
}

#[test]
fn test_cancellation_records_command_and_terminates_process_group() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let ready = fixture._temp.path().join("tidy-ready");
    let body = format!(
        "open({:?},'w').write(str(os.getpid()))\ntime.sleep(20)",
        ready.to_str().expect("ready path")
    );
    let search = fixture.install_fake_tidy(&body);
    let child = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
        .arg("--project")
        .arg(&fixture.project)
        .args(["--report", "build/report.json", "check"])
        .env("CPP_INFRA_BIN_DIR", &fixture.bin)
        .env("PATH", search)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cancelable analysis");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ready.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready.exists(), "tidy reached analysis stage");
    let tidy_pid: i32 = fs::read_to_string(&ready)
        .expect("PID file")
        .parse()
        .expect("PID integer");
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let output = child.wait_with_output().expect("cancelled process output");
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = fixture.report();
    assert_eq!(report["status"], "cancelled");
    assert!(
        report["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .any(|command| command["cancelled"] == true)
    );
    assert_eq!(
        unsafe { libc::kill(tidy_pid, 0) },
        -1,
        "cancelled tool process must be gone"
    );
}

#[test]
fn test_sarif_fingerprints_do_not_depend_on_checkout_directory() {
    let first = Fixture::new("int broken(){int*p=nullptr;return *p;}\n");
    let second = Fixture::new("int broken(){int*p=nullptr;return *p;}\n");
    let mut fingerprints = Vec::new();
    for fixture in [first, second] {
        assert_eq!(
            fixture
                .run(&["--report", "build/report.json", "check", "--sarif", "build/check.sarif"])
                .status
                .code(),
            Some(1)
        );
        let sarif: Value =
            serde_json::from_slice(&fs::read(fixture.project.join("build/check.sarif")).expect("SARIF")).expect("JSON");
        fingerprints.push(sarif["runs"][0]["results"][0]["partialFingerprints"].clone());
    }
    assert_eq!(fingerprints[0], fingerprints[1]);
}

#[test]
fn test_concurrent_build_state_change_is_rejected() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    fixture.alter_context("state=json.loads((build / '.cpp-infra-build-state.json').read_text());state['fingerprint']='other-profile-build';(build / '.cpp-infra-build-state.json').write_text(json.dumps(state))");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("state mismatch"));
}

#[test]
fn test_consumption_rechecks_complete_build_identity_before_passing() {
    for mutation in [
        "p=build/'.cpp-infra-build-state.json';s=json.loads(p.read_text());s['fingerprint']='0'*64;p.write_text(json.dumps(s))",
        "p=build/'.cpp-infra-build-state.json';s=json.loads(p.read_text());s['configuration_inputs']=[];p.write_text(json.dumps(s))",
        "p=build/'.cpp-infra-build-state.json';s=json.loads(p.read_text());s['cmake_cache_digest']='0'*64;p.write_text(json.dumps(s))",
        "p=build/'.cpp-infra-build-state.json';s=json.loads(p.read_text());s['source_excluded_paths']=['other-report.json'];p.write_text(json.dumps(s))",
        "p=build/'.cpp-infra-project.json';s=json.loads(p.read_text());s['project']='/different-project';p.write_text(json.dumps(s))",
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        let path = fixture.bin.join("clang-tidy-18");
        let script = format!(
            "#!/usr/bin/env python3\nimport pathlib,json,os,sys\nif any(a.startswith('--export-fixes=') for a in sys.argv):\n build=pathlib.Path('build/infra/analysis')\n {mutation}\nos.execv('/usr/bin/clang-tidy-18',['/usr/bin/clang-tidy-18']+sys.argv[1:])\n"
        );
        fs::write(&path, script).expect("thin real-tidy wrapper");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable wrapper");
        let output = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
            .arg("--project")
            .arg(&fixture.project)
            .args(["--report", "build/report.json", "check"])
            .env("CPP_INFRA_BIN_DIR", &fixture.bin)
            .env(
                "PATH",
                format!("{}:{}", fixture.bin.display(), std::env::var("PATH").expect("PATH")),
            )
            .output()
            .expect("consumption state mutation");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}: {}",
            mutation,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.report()["status"], "error");
        assert!(String::from_utf8_lossy(&output.stderr).contains("state mismatch"));
    }
}

#[test]
fn test_shared_run_context_produces_disjoint_artifact_paths() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(0)
    );
    let initial = fixture.report();
    let root = PathBuf::from(initial["report_root"].as_str().expect("root"));
    let context_path = root.join("shared-context.json");
    let context = serde_json::json!({"schema":1,"run_id":initial["run_id"],"task_id":"same-task","bundle_id":"development","project":fixture.project,"profile":"analysis","source_digest":initial["source_digest"],"config_digest":initial["config_digest"],"report_root":root});
    fs::write(&context_path, serde_json::to_vec(&context).expect("JSON")).expect("shared context");
    let spawn = |name: &str| {
        Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
            .arg("--project")
            .arg(&fixture.project)
            .arg("--report")
            .arg(name)
            .arg("--run-context")
            .arg(&context_path)
            .arg("check")
            .env("CPP_INFRA_BIN_DIR", &fixture.bin)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("shared run process")
    };
    let first = spawn("build/first.json");
    let second = spawn("build/second.json");
    for child in [first, second] {
        let output = child.wait_with_output().expect("run result");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let first: Value =
        serde_json::from_slice(&fs::read(fixture.project.join("build/first.json")).expect("first report"))
            .expect("first JSON");
    let second: Value =
        serde_json::from_slice(&fs::read(fixture.project.join("build/second.json")).expect("second report"))
            .expect("second JSON");
    assert_eq!(first["report_root"], second["report_root"]);
    assert_ne!(
        first["data"]["analysis_directory"],
        second["data"]["analysis_directory"]
    );
    for report in [&first, &second] {
        for artifact in report["artifacts"].as_array().expect("artifacts") {
            let path = root.join(artifact["path"].as_str().expect("relative artifact"));
            assert_eq!(
                artifact["sha256"],
                Sha256::digest(fs::read(path).expect("artifact"))
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
        }
    }
}

#[test]
fn test_prepared_context_requires_authoritative_verify_and_stale_state_is_rejected() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(0)
    );
    let initial = fixture.report();
    let root = PathBuf::from(initial["report_root"].as_str().expect("root"));
    let artifact = initial["artifacts"]
        .as_array()
        .expect("artifacts")
        .iter()
        .find(|a| a["kind"] == "build-context")
        .expect("build context");
    let context_path = root.join("reuse-context.json");
    let context = serde_json::json!({"schema":1,"run_id":initial["run_id"],"task_id":"reuse","bundle_id":"development","project":fixture.project,"profile":"analysis","source_digest":initial["source_digest"],"config_digest":initial["config_digest"],"report_root":root,"build_context":root.join(artifact["path"].as_str().expect("artifact path"))});
    fs::write(&context_path, serde_json::to_vec(&context).expect("JSON")).expect("reuse context");
    fs::write(
        fixture.bin.join("cpp-infra-verify"),
        "#!/usr/bin/env python3\nraise SystemExit(91)\n",
    )
    .expect("unavailable authority");
    let output = fixture.run(&[
        "--report",
        "build/report.json",
        "--run-context",
        context_path.to_str().expect("path"),
        "check",
    ]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(fixture.bin.join("cpp-infra-verify"), include_str!("fixtures/verify.py")).expect("restore authority");
    let output = fixture.run(&[
        "--report",
        "build/report.json",
        "--run-context",
        context_path.to_str().expect("path"),
        "check",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fixture.report()["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .any(|c| c["program"].as_str().is_some_and(|p| p.ends_with("cpp-infra-verify")))
    );
    let state_path = fixture.project.join("build/infra/analysis/.cpp-infra-build-state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).expect("state")).expect("stateJSON");
    state["fingerprint"] = serde_json::json!("concurrent-change");
    fs::write(state_path, serde_json::to_vec(&state).expect("JSON")).expect("state change");
    let output = fixture.run(&[
        "--report",
        "build/report.json",
        "--run-context",
        context_path.to_str().expect("path"),
        "check",
    ]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn test_internal_symlink_root_uses_canonical_ownership() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    std::os::unix::fs::symlink("src", fixture.project.join("source-alias")).expect("internal symlink");
    let config = fixture.project.join(".infra/project.toml");
    fs::write(
        &config,
        fs::read_to_string(&config).expect("project").replace(
            "source_roots = ['src', 'include']",
            "source_roots = ['source-alias', 'include']",
        ),
    )
    .expect("root alias");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_report_root_symlink_escape_is_rejected_before_creating_files() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let outside = fixture._temp.path().join("outside");
    fs::create_dir(&outside).expect("outside directory");
    std::os::unix::fs::symlink(&outside, fixture.project.join("build")).expect("escaping build symlink");
    let output = fixture.run(&["check"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        fs::read_dir(outside).expect("outside directory").count(),
        0,
        "rejection must happen before writes"
    );
}

#[test]
fn test_report_and_sarif_outputs_cannot_overwrite_project_inputs() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let project_config = fixture.project.join(".infra/project.toml");
    let before = fs::read(&project_config).expect("config");
    let output = fixture.run(&["--report", ".infra/project.toml", "check"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(fs::read(project_config).expect("config preserved"), before);
    let output = fixture.run(&[
        "--report",
        "build/report.json",
        "check",
        "--sarif",
        "src/analysis.sarif",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.project.join("src/analysis.sarif").exists());
}

#[test]
fn test_numeric_separators_and_prefixed_characters_cannot_hide_nolint() {
    for literal in ["0xA'B", "0xAB'CD", "0b10'01", "1'000", "1.0e1'0", "u8'A'"] {
        let source = format!("int broken() {{ constexpr auto n = {literal}; int *p=nullptr; return *p + (n > 0); }}\n");
        let fixture = Fixture::new(&source);
        let control = fixture.run(&["--report", "build/control.json", "check"]);
        assert_eq!(control.status.code(), Some(1), "real clang-tidy control: {literal}");
        let control: Value =
            serde_json::from_slice(&fs::read(fixture.project.join("build/control.json")).expect("control report"))
                .expect("control JSON");
        assert!(
            control["diagnostics"]
                .as_array()
                .expect("control diagnostics")
                .iter()
                .any(|d| d["rule"] == "clang-analyzer-core.NullDereference"),
            "real analyzer must detect the control: {literal}"
        );
        fs::write(
            fixture.project.join("src/main.cpp"),
            format!("{} // NOLINT\n", source.trim_end()),
        )
        .expect("suppressed source");
        let rejected = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            rejected.status.code(),
            Some(1),
            "default policy must reject NOLINT after {literal}"
        );
        assert!(
            fixture.report()["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["code"] == "analyze.nolint")
        );
        let policy = fixture.project.join(".infra/analyze/policy/policy.toml");
        fs::write(
            &policy,
            fs::read_to_string(&policy)
                .expect("policy")
                .replace("reject_nolint = true", "reject_nolint = false"),
        )
        .expect("allow directive presence");
        let analyzed = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            analyzed.status.code(),
            Some(1),
            "overlay must preserve mandatory checks after {literal}"
        );
        assert!(
            fixture.report()["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["rule"] == "clang-analyzer-core.NullDereference")
        );
    }
}

#[test]
fn test_runtime_snapshot_accepts_equal_content_copy_and_checks_both_files() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "plan"]).status.code(),
        Some(0)
    );
    let report = fixture.report();
    let root = PathBuf::from(report["report_root"].as_str().expect("root"));
    let binary = Path::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
        .canonicalize()
        .expect("binary");
    let copied = fixture.bin.join("snapshot-analyze-copy");
    fs::copy(&binary, &copied).expect("same-content executable copy");
    let mut snapshot = serde_json::json!({"schema":1,"bundle_id":"copy-fixture","tools":[{"name":"cpp-infra-analyze","version":"0.1.0","revision":null,"protocol":1,"binary":copied,"executable_sha256":Sha256::digest(fs::read(&binary).expect("binary bytes")).iter().map(|byte| format!("{byte:02x}")).collect::<String>()}],"offline":true,"stale":false});
    let snapshot_path = root.join("copy-snapshot.json");
    let context_path = root.join("copy-context.json");
    let write_context = |snapshot: &Value| {
        let bytes = serde_json::to_vec(snapshot).expect("snapshot JSON");
        fs::write(&snapshot_path, &bytes).expect("snapshot");
        let context = serde_json::json!({"schema":1,"run_id":report["run_id"],"task_id":"copy-check","bundle_id":"copy-fixture","project":fixture.project,"profile":"analysis","source_digest":report["source_digest"],"config_digest":report["config_digest"],"report_root":root,"runtime_snapshot":snapshot_path,"runtime_snapshot_digest":Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect::<String>()});
        fs::write(&context_path, serde_json::to_vec(&context).expect("context JSON")).expect("context");
    };
    write_context(&snapshot);
    let run = || {
        fixture.run(&[
            "--report",
            "build/report.json",
            "--run-context",
            context_path.to_str().expect("context path"),
            "plan",
        ])
    };
    let valid = run();
    assert_eq!(
        valid.status.code(),
        Some(0),
        "identical executable bytes at a different path must be accepted: {}",
        String::from_utf8_lossy(&valid.stderr)
    );
    fs::write(&copied, b"tampered executable copy").expect("tamper entry binary");
    assert_eq!(run().status.code(), Some(2), "entry.binary must match its declared SHA");
    snapshot["tools"][0]["executable_sha256"] = serde_json::json!(
        Sha256::digest(fs::read(&copied).expect("tampered bytes"))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    write_context(&snapshot);
    assert_eq!(
        run().status.code(),
        Some(2),
        "current executable must also match the same SHA"
    );
    snapshot["tools"][0]["binary"] = serde_json::json!(fixture.bin.join("missing-entry-binary"));
    write_context(&snapshot);
    assert_eq!(
        run().status.code(),
        Some(2),
        "entry.binary must exist even if current executable is valid"
    );
}

#[test]
fn test_external_output_aliases_cannot_write_into_project_source_or_configuration() {
    for destination in ["src", ".infra"] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        let alias = fixture._temp.path().join("external-alias");
        std::os::unix::fs::symlink(fixture.project.join(destination), &alias)
            .expect("external alias into project input");
        let report = alias.join("nested/new-report.json");
        let output = fixture.run(&["--report", report.to_str().expect("report path"), "check"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "external report alias must obey canonical source ownership"
        );
        assert!(
            !fixture.project.join(destination).join("nested").exists(),
            "report must not create source/config directories"
        );
        let sarif = alias.join("nested/new-analysis.sarif");
        let output = fixture.run(&[
            "--report",
            "build/report.json",
            "check",
            "--sarif",
            sarif.to_str().expect("SARIF path"),
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "external SARIF alias must obey canonical source ownership"
        );
        assert!(
            !fixture.project.join(destination).join("nested").exists(),
            "SARIF must not create source/config directories"
        );
    }
}

#[test]
fn test_project_report_exclusions_are_shared_with_verify_and_stable_across_runs() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let mut digests = Vec::new();
    for _ in 0..2 {
        let output = fixture.run(&["--report", "result.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "repeated root report must not become a verify source input: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value =
            serde_json::from_slice(&fs::read(fixture.project.join("result.json")).expect("root report")).expect("JSON");
        digests.push((report["source_digest"].clone(), report["config_digest"].clone()));
        assert_eq!(report["source_excluded_paths"], serde_json::json!(["result.json"]));
        let context = PathBuf::from(report["report_root"].as_str().expect("root"))
            .join(
                report["data"]["analysis_directory"]
                    .as_str()
                    .expect("analysis directory"),
            )
            .join("verify-run-context.json");
        let child: Value = serde_json::from_slice(&fs::read(context).expect("child context")).expect("child JSON");
        assert_eq!(child["source_excluded_paths"], report["source_excluded_paths"]);
    }
    assert_eq!(digests[0], digests[1]);
    fs::create_dir(fixture.project.join("reports")).expect("report directory");
    fs::write(fixture.project.join("reports/business.txt"), "tracked business input").expect("business input");
    let first = fixture.run(&["--report", "reports/result.json", "check"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "only report file may be excluded: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before: Value =
        serde_json::from_slice(&fs::read(fixture.project.join("reports/result.json")).expect("nested report"))
            .expect("JSON");
    fs::write(fixture.project.join("reports/business.txt"), "changed business input").expect("business input change");
    let second = fixture.run(&["--report", "reports/result.json", "check"]);
    assert_eq!(second.status.code(), Some(0));
    let after: Value =
        serde_json::from_slice(&fs::read(fixture.project.join("reports/result.json")).expect("nested report"))
            .expect("JSON");
    assert_ne!(
        before["source_digest"], after["source_digest"],
        "excluding a report cannot exclude its siblings"
    );
}

#[test]
fn test_inherited_source_exclusions_cannot_hide_inputs_or_unknown_business_json() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "plan"]).status.code(),
        Some(0)
    );
    let report = fixture.report();
    let root = PathBuf::from(report["report_root"].as_str().expect("root"));
    let context_path = root.join("excluded-context.json");
    fs::write(fixture.project.join("business.json"), "{\"business\":true}").expect("unknown business JSON");
    for path in [
        "src/main.cpp",
        ".infra/project.toml",
        "conan/result.json",
        "README.md",
        "",
        "../escape.json",
        "bad\\name.json",
        "src/",
        "business.json",
    ] {
        let context = serde_json::json!({"schema":1,"run_id":report["run_id"],"task_id":"excluded-input","bundle_id":"development","project":fixture.project,"profile":"analysis","source_digest":report["source_digest"],"config_digest":report["config_digest"],"report_root":root,"source_excluded_paths":[path]});
        fs::write(&context_path, serde_json::to_vec(&context).expect("context JSON")).expect("context");
        let output = fixture.run(&[
            "--report",
            "build/report.json",
            "--run-context",
            context_path.to_str().expect("context path"),
            "plan",
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "unsafe exclusion must be rejected before digest matching: {path}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("exclu"),
            "invalid list must not merely fail as stale context: {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_non_utf8_analysis_inputs_fail_closed_with_their_path() {
    for (relative, source) in [
        ("src/main.cpp", "int broken(){int*p=nullptr;return *p;} // NOLINT\n"),
        (
            "include/unused.hpp",
            "inline int broken(){int*p=nullptr;return *p;} // NOLINT\n",
        ),
        (
            "include/included.hpp",
            "inline int broken(){int*p=nullptr;return *p;} // NOLINT\n",
        ),
        (
            "include/extensionless",
            "inline int broken(){int*p=nullptr;return *p;} // NOLINT\n",
        ),
        (
            "third_party/vendor.hpp",
            "inline int broken(){int*p=nullptr;return *p;} // NOLINT\n",
        ),
        (
            "third_party/unused-dependency.hpp",
            "inline int broken(){int*p=nullptr;return *p;} // NOLINT\n",
        ),
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        let input = fixture.project.join(relative);
        fs::create_dir_all(input.parent().expect("input directory")).expect("input directory");
        let mut bytes = b"// non UTF-8 comment: \xff\n".to_vec();
        bytes.extend_from_slice(source.as_bytes());
        fs::write(&input, &bytes).expect("Clang-compatible non-UTF8 comment");
        if relative == "third_party/unused-dependency.hpp" {
            fs::write(
                fixture.project.join("include/unused.hpp"),
                "#include \"../third_party/unused-dependency.hpp\"\n",
            )
            .expect("uncovered public header dependency");
        } else if relative.contains("included")
            || relative.contains("extensionless")
            || relative.starts_with("third_party")
        {
            fs::write(
                fixture.project.join("src/main.cpp"),
                format!("#include \"../{relative}\"\nint clean(){{return 42;}}\n"),
            )
            .expect("including source");
        }
        let control = fixture._temp.path().join("control.cpp");
        fs::write(
            &control,
            b"// non UTF-8 comment: \xff\nint broken(){int*p=nullptr;return *p;}\n",
        )
        .expect("non-UTF8 clang-tidy control");
        let control = Command::new("/usr/bin/clang-tidy-18")
            .arg(&control)
            .args(["--checks=-*,clang-analyzer-*", "--", "-std=c++17"])
            .output()
            .expect("real clang-tidy control");
        assert!(
            String::from_utf8_lossy(&control.stdout).contains("clang-analyzer-core.NullDereference"),
            "Clang must compile and analyze the same invalid UTF8 comment"
        );
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "unsupported encoding cannot pass or be skipped for {relative}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(
            message.contains("UTF-8") && message.contains(relative),
            "encoding failure must identify its path: {message}"
        );
        assert_eq!(fixture.report()["status"], "error");
    }
    let fixture = Fixture::new("int clean(){return 42;}\n");
    fs::write(fixture.project.join("src/asset.bin"), [0xff, 0xfe, 0]).expect("unselected binary asset");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(0),
        "non-code assets must remain outside source encoding validation"
    );
}

#[test]
fn test_spliced_comment_cannot_hide_nolint_from_policy_or_overlay() {
    for splice in ["\\\n", "\\\r\n"] {
        let source = format!("// continued comment {splice}\"\nint broken(){{int*p=nullptr;return *p;}}\n");
        let fixture = Fixture::new(&source);
        let control = fixture.run(&["--report", "build/control.json", "check"]);
        assert_eq!(
            control.status.code(),
            Some(1),
            "real clang-tidy must detect the splice control"
        );
        fs::write(
            fixture.project.join("src/main.cpp"),
            format!("{} // NOLINT\n", source.trim_end()),
        )
        .expect("spliced NOLINT source");
        let rejected = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            rejected.status.code(),
            Some(1),
            "spliced comment cannot hide a mandatory suppression directive"
        );
        assert!(
            fixture.report()["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["code"] == "analyze.nolint")
        );
        let policy = fixture.project.join(".infra/analyze/policy/policy.toml");
        fs::write(
            &policy,
            fs::read_to_string(&policy)
                .expect("policy")
                .replace("reject_nolint = true", "reject_nolint = false"),
        )
        .expect("allow NOLINT presence");
        let overlay = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(overlay.status.code(), Some(1));
        assert!(
            fixture.report()["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["rule"] == "clang-analyzer-core.NullDereference")
        );
    }
}

#[test]
fn test_configuration_inputs_cannot_enter_digest_excluded_zones() {
    for zone in [
        "build",
        ".git",
        "target",
        ".infra/.tmp",
        ".infra/transactions",
        ".infra/.transactions",
    ] {
        for relative in [
            ".infra/project.toml",
            ".infra/analyze/config.toml",
            ".infra/analyze/policy/policy.toml",
            ".infra/analyze/policy/clang-tidy.yaml",
        ] {
            let fixture = Fixture::new("int clean(){return 42;}\n");
            let input = fixture.project.join(relative);
            let hidden = fixture
                .project
                .join(zone)
                .join(input.file_name().expect("config filename"));
            fs::create_dir_all(hidden.parent().expect("hidden directory")).expect("excluded directory");
            fs::write(&hidden, fs::read(&input).expect("original configuration")).expect("excluded configuration copy");
            fs::remove_file(&input).expect("replace disposable fixture file");
            std::os::unix::fs::symlink(&hidden, &input).expect("configuration link into excluded zone");
            let output = fixture.run(&["--report", "build/report.json", "plan"]);
            assert_eq!(
                output.status.code(),
                Some(2),
                "consumed configuration cannot be untracked: {relative} -> {zone}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let fixture = Fixture::new("int clean(){return 42;}\n");
        let exception = fixture.project.join(zone).join("exceptions.toml");
        fs::create_dir_all(exception.parent().expect("exception directory")).expect("excluded directory");
        fs::write(&exception, "schema = 1\nexceptions = []\n").expect("untracked exceptions");
        let config = fixture.project.join(".infra/analyze/config.toml");
        fs::write(
            &config,
            format!(
                "{}\nexceptions = '{zone}/exceptions.toml'\n",
                fs::read_to_string(&config).expect("config")
            ),
        )
        .expect("exception reference");
        let output = fixture.run(&["--report", "build/report.json", "plan"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "direct exceptions in excluded zone must fail: {zone}"
        );
    }
}

#[test]
fn test_fresh_rejects_policy_retargeted_into_excluded_build() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let policy = fixture.project.join(".infra/analyze/policy/policy.toml");
    let tracked = fixture.project.join(".infra/analyze/policy/build/tracked-policy.toml");
    fs::create_dir_all(tracked.parent().expect("tracked directory")).expect("nested build directory");
    fs::write(&tracked, fs::read(&policy).expect("policy")).expect("tracked policy");
    fs::remove_file(&policy).expect("replace disposable policy fixture");
    std::os::unix::fs::symlink(&tracked, &policy).expect("tracked internal policy link");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "plan"]).status.code(),
        Some(0),
        "nested build components are valid tracked configuration"
    );
    fixture.alter_context("policy=project / '.infra/analyze/policy/policy.toml';hidden=project / 'build/retargeted-policy.toml';hidden.write_bytes(policy.read_bytes());policy.unlink();policy.symlink_to(hidden)");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "fresh must reject a symlink-only change whose content digests are unchanged: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("excluded"));
}

#[test]
fn test_raw_string_line_splices_do_not_create_false_directives() {
    let fixture = Fixture::new("const char* text(){return R\"tag(ignored \\\n)ta\\\ng\" // NOLINT\n)tag\";}\n");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "raw string contents retain physical splices: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_source_exclusions_reject_raw_aliases_before_file_state_checks() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "plan"]).status.code(),
        Some(0)
    );
    let old = fixture.report();
    let root = PathBuf::from(old["report_root"].as_str().expect("root"));
    let context_path = root.join("raw-exclusion-context.json");
    fs::write(fixture.project.join("business.json"), "{\"new_business_input\":true}")
        .expect("new input after old context");
    for raw in [
        "business.json/",
        "business.json//",
        "./business.json",
        "nested/../business.json",
        "nested/./business.json",
        "nested//business.json",
        "/business.json",
        "business\\data.json",
        "C:/business.json",
        "C:business.json",
        "c:/business.json",
    ] {
        let context = serde_json::json!({"schema":1,"run_id":old["run_id"],"task_id":"raw-exclusion","bundle_id":"development","project":fixture.project,"profile":"analysis","source_digest":old["source_digest"],"config_digest":old["config_digest"],"report_root":root,"source_excluded_paths":[raw]});
        fs::write(&context_path, serde_json::to_vec(&context).expect("context JSON")).expect("context");
        let output = fixture.run(&[
            "--report",
            "build/report.json",
            "--run-context",
            context_path.to_str().expect("context path"),
            "plan",
        ]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "raw aliases cannot hide business input: {raw}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid source exclusion report path"),
            "raw validation must precede exists, role and stale-digest checks for {raw}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.project.join("business.json")).expect("input unchanged"),
        "{\"new_business_input\":true}"
    );
}

#[test]
fn test_preprocessing_header_names_cannot_hide_later_nolint_comments() {
    for header in ["it's.h", "it\"s.h"] {
        let source = format!("#if __has_include(<{header}>)\n#endif\nint broken(){{int*p=nullptr;return *p;}}\n");
        let fixture = Fixture::new(&source);
        assert_eq!(
            fixture.run(&["--report", "build/control.json", "check"]).status.code(),
            Some(1)
        );
        let control: Value =
            serde_json::from_slice(&fs::read(fixture.project.join("build/control.json")).expect("control"))
                .expect("JSON");
        assert!(
            control["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["rule"] == "clang-analyzer-core.NullDereference")
        );
        fs::write(
            fixture.project.join("src/main.cpp"),
            format!("{} // NOLINT\n", source.trim_end()),
        )
        .expect("NOLINT source");
        assert_eq!(
            fixture.run(&["--report", "build/report.json", "check"]).status.code(),
            Some(1),
            "header-name must not consume the subsequent comment: {header}"
        );
        assert!(
            fixture.report()["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["code"] == "analyze.nolint")
        );
        let policy = fixture.project.join(".infra/analyze/policy/policy.toml");
        fs::write(
            &policy,
            fs::read_to_string(&policy)
                .expect("policy")
                .replace("reject_nolint = true", "reject_nolint = false"),
        )
        .expect("allow NOLINT presence");
        assert_eq!(
            fixture.run(&["--report", "build/report.json", "check"]).status.code(),
            Some(1)
        );
        assert!(
            fixture.report()["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|d| d["rule"] == "clang-analyzer-core.NullDereference")
        );
    }
}

#[test]
fn test_backend_nolint_suppression_in_literals_or_identifiers_fails_closed() {
    for declaration in [
        "const char* text=\"NOLINT\"; (void)text;",
        "int NOLINT=1; (void)NOLINT;",
    ] {
        let fixture = Fixture::new(&format!("int broken(){{int*p=nullptr;{declaration}return *p;}}\n"));
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "backend's raw-line suppression must not yield passed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.report()["status"], "error");
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot certify"));
    }
    for declaration in [
        "const char* label(){return \"NOLINT\";}\n",
        "int label(){int NOLINT=1;return NOLINT;}\n",
        "#define LABEL \"NOLINT\"\nconst char* label(){return LABEL;}\n",
    ] {
        let fixture = Fixture::new(declaration);
        assert_eq!(
            fixture.run(&["--report", "build/report.json", "check"]).status.code(),
            Some(0),
            "a non-comment token with no suppressed diagnostics is valid"
        );
    }
}

#[test]
fn test_ambiguous_raw_literal_inside_header_name_is_not_certified() {
    for prefix in [
        "#if __has_include(<R\"(>)",
        "#define HAS __has_include\n#if HAS(<R\"(>)",
    ] {
        let source = format!("{prefix}\n#endif\nint broken(){{int*p=nullptr;return *p;}} // NOLINT\n// )\"\n");
        let fixture = Fixture::new(&source);
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "a raw lexical literal cannot certify preprocessing header-name contents"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("classif"));
    }
}

#[test]
fn test_sarif_cannot_create_reserved_project_inputs_or_unknown_files() {
    for reserved in [
        "CMakeUserPresets.json",
        "CMakePresets.json",
        "conanfile.py",
        "conanfile.txt",
        "README.md",
        "README.zh_CN.md",
        "conan/new/output.sarif",
        "cmake/new/output.sarif",
        ".git/new/output.sarif",
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        let target = fixture.project.join(reserved);
        let output = fixture.run(&["--report", "build/report.json", "check", "--sarif", reserved]);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            !target.exists(),
            "reserved input must never be created before rejection: {reserved}"
        );
        if reserved.contains("/new/") {
            assert!(
                !target.parent().expect("parent").exists(),
                "reserved parent must not be created"
            );
        }
        let alias = fixture._temp.path().join("project-alias");
        std::os::unix::fs::symlink(&fixture.project, &alias).expect("canonical root alias");
        let alias_target = alias.join(reserved);
        assert_eq!(
            fixture
                .run(&[
                    "--report",
                    "build/report.json",
                    "check",
                    "--sarif",
                    alias_target.to_str().expect("alias")
                ])
                .status
                .code(),
            Some(2)
        );
        assert!(!target.exists());
    }
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let business = fixture.project.join("business.json");
    fs::write(&business, "{\"business\":true}").expect("business input");
    assert_eq!(
        fixture
            .run(&["--report", "build/report.json", "check", "--sarif", "business.json"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        fs::read_to_string(business).expect("business preserved"),
        "{\"business\":true}"
    );
}

#[test]
fn test_libclang_doctor_reports_runtime_and_missing_override_fails() {
    let output = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
        .args(["doctor", "--json"])
        .output()
        .expect("doctor");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    assert!(report["libclang"]["version"].as_str().expect("version").contains("18."));
    assert!(Path::new(report["libclang"]["path"].as_str().expect("runtime path")).is_file());
    let output = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
        .args(["doctor", "--json"])
        .env("CPP_INFRA_LIBCLANG", "/missing/libclang-18.so")
        .output()
        .expect("missing runtime probe");
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("CPP_INFRA_LIBCLANG"));
}

#[test]
fn test_native_nolint_handling_preserves_third_party_ownership() {
    for (source, expected) in [
        ("inline int vendor(){int*p=nullptr;return *p;} // NOLINT\n", 0),
        (
            "inline int vendor(){const char*marker=\"NOLINT\";(void)marker;int*p=nullptr;return *p;}\n",
            2,
        ),
    ] {
        let fixture = Fixture::new("#include \"../third_party/vendor.hpp\"\nint clean(){return 42;}\n");
        fs::create_dir(fixture.project.join("third_party")).expect("vendor directory");
        fs::write(fixture.project.join("third_party/vendor.hpp"), source).expect("vendor input");
        let config = fixture.project.join(".infra/project.toml");
        fs::write(
            &config,
            fs::read_to_string(&config)
                .expect("project")
                .replace("third_party_roots = []", "third_party_roots = ['third_party']"),
        )
        .expect("vendor ownership");
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            fixture.report()["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .all(|d| d["severity"] != "warning" && d["severity"] != "error"),
            "pure vendor diagnostics must not become owned gate failures"
        );
        if expected == 2 {
            assert_eq!(fixture.report()["status"], "error");
            assert!(String::from_utf8_lossy(&output.stderr).contains("cannot certify"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("ownership cannot be inferred"));
        } else {
            assert_eq!(fixture.report()["status"], "passed");
            assert!(
                fixture.report()["diagnostics"]
                    .as_array()
                    .expect("notes")
                    .iter()
                    .any(|d| d["owned"] == false && d["code"] == "analyze.nolint")
            );
        }
    }
}

#[test]
fn test_native_lexer_retains_legal_literal_and_macro_contexts() {
    for literal in [
        "\"NOLINT\"",
        "u8\"NOLINT\"",
        "L\"NOLINT\"",
        "R\"tag(NOLINT)tag\"",
        "u8R\"tag(NOLINT)tag\"",
    ] {
        let fixture = Fixture::new(&format!("auto label(){{return {literal};}}\n"));
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "legal non-comment literal {literal}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let fixture = Fixture::new("#define NOLINT 1\nint label(){return NOLINT;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(0)
    );
    let fixture = Fixture::new("#if __has_include(<it's.NOLINT.h>)\n#endif\nint clean(){return 42;}\n");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown raw token contents cannot certify a preprocessing header-name"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("classif"));
}

#[test]
fn test_quoted_header_queries_and_pragma_messages_remain_valid_literals() {
    for source in [
        "#if __has_include(\"NOLINT.hpp\")\n#endif\nint clean(){return 42;}\n",
        "#if 1 < 2 && __has_include(\"NOLINT.hpp\")\n#endif\nint clean(){return 42;}\n",
        "#pragma message(\"NOLINT\")\nint clean(){return 42;}\n",
        "#define MSG \"NOLINT\"\n#pragma message(MSG)\nint clean(){return 42;}\n",
    ] {
        let fixture = Fixture::new(source);
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "ordinary PP literals must be supported: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.report()["status"], "passed");
    }
    let fixture = Fixture::new("int broken(){_Pragma(\"message(\\\"NOLINT\\\")\") int*p=nullptr;return *p;}\n");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "excluding informational pragma messages cannot hide a mandatory null dereference"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot certify"));
}

#[test]
fn test_explicit_pragma_diagnostic_policy_is_never_disabled() {
    let fixture = Fixture::new("#pragma message(\"ordinary\")\nint clean(){return 42;}\n");
    fs::write(
        fixture.project.join(".infra/analyze/policy/clang-tidy.yaml"),
        "Checks: '-*,clang-diagnostic-#pragma-messages,clang-analyzer-*,bugprone-*'\nHeaderFilterRegex: '.*'\n",
    )
    .expect("stricter compiler diagnostic policy");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        fixture.report()["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["rule"] == "clang-diagnostic-#pragma-messages")
    );
    fs::write(
        fixture.project.join("src/main.cpp"),
        "#pragma message(\"NOLINT\")\nint clean(){return 42;}\n",
    )
    .expect("self-suppressing policy warning");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "selected compiler diagnostic cannot be silently disabled"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot certify"));
}

#[test]
fn test_build_configuration_proof_is_required_and_rechecked_under_consumption_lock() {
    for alteration in [
        "context.pop('configuration_inputs',None);context.pop('cmake_cache_digest',None)",
        "p=next(build.glob('CMakeFiles/*/CMakeCXXCompiler.cmake'));context['configuration_inputs']=[dict(path=str(p.resolve()),sha256=hashlib.sha256(p.read_bytes()).hexdigest(),kind='cmake-generated')];context['cmake_cache_digest']=hashlib.sha256((build/'CMakeCache.txt').read_bytes()).hexdigest();p.write_text(p.read_text()+'\\n# changed after preparation\\n')",
        "context['configuration_inputs']=[dict(path=str((project/'CMakeLists.txt').resolve()),sha256=hashlib.sha256((project/'CMakeLists.txt').read_bytes()).hexdigest(),kind='project')];context['cmake_cache_digest']=hashlib.sha256((build/'CMakeCache.txt').read_bytes()).hexdigest();p=build/'CMakeCache.txt';p.write_text(p.read_text()+'\\n# changed after preparation\\n')",
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        fixture.alter_context(alteration);
        let output = fixture.run(&["--report", "build/report.json", "check"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "missing/stale actual configuration evidence cannot pass: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fixture.report()["status"], "error");
    }
}

#[test]
fn test_cli_rejects_missing_inputs_and_observes_resource_defaults() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    assert_eq!(fixture.run(&["--report", "build/report.json"]).status.code(), Some(2));
    let config = fixture.project.join(".infra/analyze/config.toml");
    let contents = fs::read_to_string(&config).expect("config");
    fs::write(
        &config,
        contents
            .replace("jobs = 2\n", "")
            .replace("timeout_per_translation_unit_seconds = 30\n", ""),
    )
    .expect("default budgets");
    let plan = fixture.run(&["plan"]);
    assert_eq!(plan.status.code(), Some(0));
    let plan: Value = serde_json::from_slice(&plan.stdout).expect("plan JSON");
    assert_eq!(plan["nodes"][1]["timeout_seconds"], 180);
    assert_eq!(
        fixture
            .run(&["--report", "build/report.json", "plan", "--profile", "missing"])
            .status
            .code(),
        Some(2)
    );
    let output = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
        .arg("--project")
        .arg(fixture._temp.path().join("missing-project"))
        .arg("check")
        .output()
        .expect("missing project");
    assert_eq!(output.status.code(), Some(3));
    fs::write(fixture.project.join("existing-input.txt"), "unknown business input").expect("plain input");
    assert_eq!(
        fixture.run(&["--report", "existing-input.txt", "plan"]).status.code(),
        Some(2)
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("existing-input.txt")).expect("unchanged input"),
        "unknown business input"
    );
    fs::remove_file(config).expect("remove disposable config");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "plan"]).status.code(),
        Some(2)
    );
}

#[test]
fn test_malformed_run_context_fields_are_rejected_as_protocol_errors() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "plan"]).status.code(),
        Some(0)
    );
    let initial = fixture.report();
    let root = PathBuf::from(initial["report_root"].as_str().expect("root"));
    let path = root.join("malformed-context.json");
    let base = serde_json::json!({"schema":1,"run_id":initial["run_id"],"task_id":"malformed","bundle_id":"development","project":fixture.project,"profile":"analysis","source_digest":initial["source_digest"],"config_digest":initial["config_digest"],"report_root":root});
    for (field, value, expected) in [
        ("project", Value::Null, "project missing"),
        ("profile", Value::Null, "profile missing"),
        ("build_context", serde_json::json!(4), "build_context must"),
        ("runtime_snapshot", serde_json::json!(4), "runtime_snapshot must"),
        (
            "runtime_snapshot_digest",
            serde_json::json!(false),
            "snapshot_digest missing",
        ),
        (
            "source_excluded_paths",
            serde_json::json!({"bad":"shape"}),
            "invalid source exclusions",
        ),
    ] {
        let mut context = base.clone();
        context[field] = value;
        if field == "runtime_snapshot" {
            context["runtime_snapshot_digest"] = serde_json::json!("not-used");
        }
        if field == "runtime_snapshot_digest" {
            context["runtime_snapshot"] = serde_json::json!(root.join("unused.json"));
        }
        fs::write(&path, serde_json::to_vec(&context).expect("context JSON")).expect("context");
        let output = fixture.run(&[
            "--report",
            "build/report.json",
            "--run-context",
            path.to_str().expect("path"),
            "plan",
        ]);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{field}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let snapshot_path = root.join("bad-runtime.json");
    for snapshot in [
        serde_json::json!({"schema":1,"bundle_id":"development","tools":null}),
        serde_json::json!({"schema":1,"bundle_id":"development","tools":[{"name":"cpp-infra-analyze","binary":null}]}),
    ] {
        let bytes = serde_json::to_vec(&snapshot).expect("snapshot JSON");
        fs::write(&snapshot_path, &bytes).expect("snapshot");
        let mut context = base.clone();
        context["runtime_snapshot"] = serde_json::json!(snapshot_path);
        context["runtime_snapshot_digest"] = serde_json::json!(
            Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        fs::write(&path, serde_json::to_vec(&context).expect("JSON")).expect("context");
        assert_eq!(
            fixture
                .run(&[
                    "--report",
                    "build/report.json",
                    "--run-context",
                    path.to_str().expect("path"),
                    "plan"
                ])
                .status
                .code(),
            Some(2)
        );
    }
}

#[test]
fn test_non_utf8_output_path_and_nonexecutable_verify_fail_without_writes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let path = fixture.project.join(OsString::from_vec(b"result-\xff.json".to_vec()));
    let output = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
        .arg("--project")
        .arg(&fixture.project)
        .arg("--report")
        .arg(&path)
        .arg("plan")
        .output()
        .expect("non-UTF8 output");
    assert_eq!(output.status.code(), Some(2));
    assert!(!path.exists());
    fs::set_permissions(fixture.bin.join("cpp-infra-verify"), fs::Permissions::from_mode(0o644))
        .expect("nonexecutable fixture");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot spawn"));
}

#[test]
fn test_target_selection_preserves_test_and_generated_source_roles() {
    for (directory, root_key) in [("tests", "test_roots"), ("generated", "generated_roots")] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        fs::create_dir(fixture.project.join(directory)).expect("source role directory");
        fs::write(
            fixture.project.join(directory).join("extra.cpp"),
            "int extra(){return 42;}\n",
        )
        .expect("additional source");
        let config = fixture.project.join(".infra/project.toml");
        fs::write(
            &config,
            fs::read_to_string(&config)
                .expect("project")
                .replace(&format!("{root_key} = []"), &format!("{root_key} = ['{directory}']")),
        )
        .expect("source role");
        let cmake = fixture.project.join("CMakeLists.txt");
        fs::write(
            &cmake,
            fs::read_to_string(&cmake).expect("cmake").replace(
                "STATIC src/main.cpp",
                &format!("STATIC src/main.cpp {directory}/extra.cpp"),
            ),
        )
        .expect("target role source");
        assert_eq!(
            fixture.run(&["--report", "build/report.json", "check"]).status.code(),
            Some(0)
        );
        assert_eq!(fixture.report()["data"]["translation_units"], 1);
        if directory == "tests" {
            let config = fixture.project.join(".infra/analyze/config.toml");
            fs::write(
                &config,
                fs::read_to_string(&config)
                    .expect("analysis")
                    .replace("include_tests = false", "include_tests = true"),
            )
            .expect("include test sources");
            assert_eq!(
                fixture.run(&["--report", "build/report.json", "check"]).status.code(),
                Some(0)
            );
            assert_eq!(fixture.report()["data"]["translation_units"], 2);
        }
    }
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let config = fixture.project.join(".infra/analyze/config.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .expect("analysis")
            .replace("['example']", "['absent']"),
    )
    .expect("missing target");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("target absent"));
}

#[test]
fn test_warning_message_digest_exception_requires_exact_supported_digest() {
    let fixture = Fixture::new("#include <algorithm>\nint clean(){int a[2]={};std::remove(a,a+2,1);return 42;}\n");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(1)
    );
    let report = fixture.report();
    let message = report["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| d["rule"] == "bugprone-unused-return-value")
        .expect("real warning")["message"]
        .as_str()
        .expect("message");
    let config = fixture.project.join(".infra/analyze/config.toml");
    fs::write(
        &config,
        format!(
            "{}\nexceptions = '.infra/analyze/exceptions.toml'\n",
            fs::read_to_string(&config).expect("analysis")
        ),
    )
    .expect("exception reference");
    for (digest, expected) in [
        (
            Sha256::digest(message.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            0,
        ),
        ("0".repeat(64), 1),
        ("z".repeat(64), 2),
    ] {
        fs::write(fixture.project.join(".infra/analyze/exceptions.toml"),format!("schema = 1\n[[exceptions]]\nrule = 'bugprone-unused-return-value'\npath = 'src/main.cpp'\nreason = 'Reviewed exact warning'\nmessage_digest = '{digest}'\n")).expect("digest exception");
        assert_eq!(
            fixture.run(&["--report", "build/report.json", "check"]).status.code(),
            Some(expected)
        );
    }
}

#[test]
fn test_success_exit_with_unparseable_or_invalid_diagnostic_export_is_an_error() {
    for (body, message) in [
        ("print('warning: missing exported diagnostics')", "without parseable"),
        (
            "p=next(a.split('=',1)[1] for a in sys.argv if a.startswith('--export-fixes='));open(p,'w').write('Diagnostics: [');print('warning: malformed export')",
            "unsupported clang-tidy",
        ),
        (
            "import json\np=next(a.split('=',1)[1] for a in sys.argv if a.startswith('--export-fixes='));open(p,'w').write(json.dumps({'Diagnostics':[{'DiagnosticName':'clang-analyzer-core.NullDereference','DiagnosticMessage':{'Message':'bad path','FilePath':'/missing/diagnostic.cpp','FileOffset':0},'Level':'Warning'}]}))",
            "path cannot be resolved",
        ),
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        let output = fixture.run_with_fake_tidy(body);
        assert_eq!(output.status.code(), Some(3));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_effective_check_configuration_must_be_parseable_and_complete() {
    for (dump, expected) in [
        ("Checks: [", "invalid effective"),
        ("WarningsAsErrors: ''", "Checks missing"),
    ] {
        let fixture = Fixture::new("int clean(){return 42;}\n");
        let search = fixture.install_fake_tidy("pass");
        let path = fixture.bin.join("clang-tidy-18");
        fs::write(
            &path,
            fs::read_to_string(&path)
                .expect("tidy script")
                .replace("Checks: '-*,clang-analyzer-*,bugprone-*'", dump),
        )
        .expect("invalid effective config");
        let output = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
            .arg("--project")
            .arg(&fixture.project)
            .args(["--report", "build/report.json", "check"])
            .env("CPP_INFRA_BIN_DIR", &fixture.bin)
            .env("PATH", search)
            .output()
            .expect("configuration fault");
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
}

#[test]
fn test_build_state_optional_exclusions_and_required_compiler_are_distinct() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    fixture.alter_context("p=build/'.cpp-infra-build-state.json';s=json.loads(p.read_text());s.pop('source_excluded_paths');p.write_text(json.dumps(s))");
    assert_eq!(
        fixture.run(&["--report", "build/report.json", "check"]).status.code(),
        Some(0)
    );
    let fixture = Fixture::new("int clean(){return 42;}\n");
    fixture.alter_context("p=build/'CMakeCache.txt';p.write_text('\\n'.join(l for l in p.read_text().splitlines() if not l.startswith('CMAKE_CXX_COMPILER:')));context['cmake_cache_digest']=sha(p);p=build/'.cpp-infra-build-state.json';s=json.loads(p.read_text());s['cmake_cache_digest']=context['cmake_cache_digest'];p.write_text(json.dumps(s))");
    let output = fixture.run(&["--report", "build/report.json", "check"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache has no C++ compiler"));
}

#[test]
fn test_source_revision_accepts_only_complete_successful_git_identity() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let path = fixture.bin.join("git");
    fs::write(&path, "#!/usr/bin/env python3\nprint('a'*40)\n").expect("controlled git identity");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("git executable");
    let output = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
        .arg("--project")
        .arg(&fixture.project)
        .args(["--report", "build/report.json", "check"])
        .env("CPP_INFRA_BIN_DIR", &fixture.bin)
        .env(
            "PATH",
            format!("{}:{}", fixture.bin.display(), std::env::var("PATH").expect("PATH")),
        )
        .output()
        .expect("revision invocation");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(fixture.report()["source_revision"], "a".repeat(40));
}

#[test]
fn test_libclang_runtime_rejects_missing_symbols_and_empty_version() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let source = fixture.bin.join("invalid-runtime.c");
    let library = fixture.bin.join("invalid-runtime.so");
    for (code, message) in [
        ("void unrelated(void) {}", "lacks clang_getClangVersion"),
        (
            "typedef struct {const void *data;unsigned flags;} CXString; CXString clang_getClangVersion(void){CXString s={0,0};return s;} const char* clang_getCString(CXString s){return 0;} void clang_disposeString(CXString s){}",
            "version is empty",
        ),
    ] {
        fs::write(&source, code).expect("fault runtime source");
        assert!(
            Command::new("/usr/bin/clang-18")
                .args(["-shared", "-fPIC"])
                .arg(&source)
                .arg("-o")
                .arg(&library)
                .status()
                .expect("build tiny fault runtime")
                .success()
        );
        let output = Command::new(env!("CARGO_BIN_EXE_cpp-infra-analyze"))
            .args(["doctor", "--json"])
            .env("CPP_INFRA_LIBCLANG", &library)
            .output()
            .expect("fault runtime probe");
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_nonzero_tidy_exit_with_only_warning_retains_process_failure() {
    let fixture = Fixture::new("int clean(){return 42;}\n");
    let output=fixture.run_with_fake_tidy("import json\np=next(a.split('=',1)[1] for a in sys.argv if a.startswith('--export-fixes='));open(p,'w').write(json.dumps({'Diagnostics':[{'DiagnosticName':'bugprone-unused-return-value','DiagnosticMessage':{'Message':'controlled warning','FilePath':'src/main.cpp','FileOffset':0},'Level':'Warning'}]}));sys.exit(1)");
    assert_eq!(output.status.code(), Some(1));
    let report = fixture.report();
    assert!(
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["rule"] == "analyze.process_failure")
    );
    assert!(
        report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|diagnostic| diagnostic["message"] == "controlled warning")
    );
}

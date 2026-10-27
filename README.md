# cpp-infra-analyze

[![Rust 1.94](https://img.shields.io/badge/Rust-1.94-orange)](Cargo.toml) [![Edition 2024](https://img.shields.io/badge/edition-2024-blue)](Cargo.toml) [![Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE) [![Clang 18](https://img.shields.io/badge/Clang-18-green)](conf/policy/clang-tidy.yaml) [![Protocol 1](https://img.shields.io/badge/protocol-1-blue)](src/reporting.rs) [![简体中文](https://img.shields.io/badge/语言-简体中文-red)](README.zh_CN.md)

When a C++ library builds successfully but a public header still dereferences a null pointer, ordinary compilation can miss the defect. `cpp-infra-analyze` prepares the analysis build through `cpp-infra-verify`, then runs clang-tidy 18 with mandatory Clang analyzer and bugprone checks. A diagnostic fails the gate unless an exact, policy-approved warning exception applies.

## Requirements and setup

The first release supports Linux, Clang/clang-tidy 18, the libclang 18 shared runtime, CMake, and single-configuration Ninja. No Clang SDK headers or link-time Clang library are required to build this Rust crate. Install the tools from the same cpp-infra tool set; `CPP_INFRA_BIN_DIR` selects its executables. If that variable is present, a missing verify executable fails without falling back to PATH. Standalone development builds use bundle `development`; `--version --json` reports a null revision unless `CPP_INFRA_BUILD_REVISION` was set when building.

```sh
cargo build --locked --release
cpp-infra-analyze --version --json
cpp-infra-analyze doctor --json
```

`doctor` reports the actual libclang file and version. `CPP_INFRA_LIBCLANG` can select its shared library explicitly; a missing override fails without fallback. Otherwise discovery checks the LLVM 18 install location and system libclang sonames, always requiring version 18.x. A missing runtime returns infrastructure error `3`; an incompatible version returns configuration error `2`. Checks require this runtime and never fall back to a handwritten C++ lexer.

The target project owns `.infra/project.toml`, CMake presets, and `.infra/analyze/config.toml`. Its analysis profile must use Clang 18 and Ninja, disable Unity builds, and enable `CMAKE_EXPORT_COMPILE_COMMANDS`. The updater delivers the two managed policy files from `conf/manifest.json`. The configuration examples in `conf/examples/` are project-owned examples and are deliberately absent from the managed manifest.

```toml
schema = 1
profile = "analysis"
targets = ["example", "example-tests"]
include_tests = true
jobs = 4
timeout_per_translation_unit_seconds = 180
additive_checks = []
```

## Run analysis

```sh
cpp-infra-analyze --project . plan --profile analysis
cpp-infra-analyze --project . --report build/analyze-report.json check --profile analysis --sarif build/analysis.sarif
```

`plan` emits preparation and runtime discovery nodes without running external commands. `check` invokes `verify prepare --profile NAME --output BUILD_CONTEXT_JSON`, validates the returned input digests, target model, compiler, generator, management marker and build state, then holds the build directory lock while consuming it. Generated headers are built before analysis. `--offline` propagates to verify. `--run-context` validates project, source/config digests and profile; an attached runtime snapshot must match its SHA-256, bundle and the running executable, and is preserved when invoking verify.

The snapshot's declared binary and the running executable must each match its recorded SHA-256. Identical copies at different paths are supported. A project-local report outside `build/` or `target/` must be a non-input JSON report; its exact relative path is recorded in `source_excluded_paths` and shared with verify. Only those files are excluded from the source digest, and configuration digests remain unaffected. Source/configuration paths and unknown existing business files cannot be used as outputs, including through external directory aliases.

An orchestrator can provide an absolute `build_context` path inside the shared report root in the run-context file. Analysis asks verify to authenticate that context before taking the consumption lock. A valid reuse performs no configure or build; verify checks actual CMake configuration inputs, dependency evidence and the cache digest. Analysis then rechecks those file hashes and build state while holding the lock, including after analysis. Contexts without configuration evidence are rejected. Per-call artifacts live under `analyze/<unique-call-id>/` inside that shared root, so concurrent tasks retain separate logs and contexts. After a failed check, read the JSON `diagnostics` array and inspect the relative locations in SARIF; after fixing the source, repeat `check` to analyze the new source digest.

Every selected implementation source needs a real compilation command. Distinct commands for the same source remain separate, including different macro definitions. The compiler discovers each TU's dependency closure. A public header under a source root that has no selected includer gets an independent include TU derived from the selected targets' real compilation requirements. An unusable header context fails explicitly. Templates are analyzed only along instantiated paths. No source-only analysis cache is used.

Analyzed C/C++ sources, public headers and included dependencies must be readable UTF-8. Unsupported encoding returns configuration error `2` with the input path; read failures cannot silently skip policy checks. Unselected binary assets are unaffected. NOLINT classification uses the stable libclang lexical C API in isolated helper processes with timeout and cancellation. Native physical spelling ranges preserve byte offsets, including line splices and raw strings. Raw lexical tokens cannot certify every preprocessing header-name context; ambiguous candidates return explicit error `2` instead of being treated as absent.

Each TU uses a separate compilation database and exported YAML diagnostic file. Jobs are bounded by `jobs`; timeout and cancellation terminate the child process group. JSON diagnostics include relative paths, severity, rule, line/column, analyzer notes and origin contexts. Equal header diagnostics merge only for equal compilation requirements. Third-party warnings are filtered after canonical path ownership checks; compilation errors still fail. Raw logs and exported diagnostics remain in `build/infra-runs/<run-id>/` with SHA-256 artifact records. SARIF 2.1.0 uses project-relative artifact URIs and stable fingerprints.

Project-local SARIF outputs must be under `build/` or `target/`; an external output is also supported. Reserved CMake, Conan, README and configuration paths are rejected before writing, including canonical aliases. Existing unknown business files cannot be overwritten.

## Policy and exceptions

Project `additive_checks` can enable additional known clang-tidy checks. Negative check patterns and arbitrary CLI injection are rejected. Mandatory checks are enabled explicitly alongside the managed configuration. Configuration TOML rejects unknown fields, unsupported schemas, duplicate targets and overlapping or escaping roots.

Configuration, policy and exception files must remain in tracked project locations. Both their declared and canonical paths are checked before reading and when checking freshness; root `.git`, `build`, `target` and the three infrastructure transaction directories cannot supply policy inputs. Runtime contexts and verified build artifacts keep their separate artifact validation.

To allow a warning that the organization policy lists in `exception_rules`, set `exceptions` to a project-relative TOML file:

```toml
schema = 1
[[exceptions]]
rule = "bugprone-unused-return-value"
path = "src/example.cpp"
reason = "The caller intentionally discards the result; reviewed in ISSUE-123."
# message_digest = "<SHA-256 of the exact diagnostic message>"
```

Exceptions match the exact rule and relative path, with an optional exact message digest. Suppressed diagnostics remain in JSON and SARIF. Errors, crashes, timeouts and invalid configuration cannot be suppressed. Owned NOLINT comments are rejected by the default policy. When policy permits their presence, native comment ranges are neutralized in a VFS overlay; source files are unchanged. Dependency comments are also neutralized for complete collection, while dependency warnings retain their normal ownership filter.

Clang-tidy can additionally suppress diagnostics when the raw source line contains NOLINT inside a literal or identifier. Those tokens are preserved to avoid changing program semantics. If backend statistics still report NOLINT suppression, the tool returns error `2` because complete results and ownership cannot be certified; it does not invent an owned diagnostic. A literal or identifier with no suppressed diagnostics remains valid.

Exit codes are `0` passed, `1` gate/build failure, `2` configuration/protocol/platform error, `3` filesystem/spawn/unparseable-tool failure, and `130` cancellation. A report is attempted for failures as well as success. A check never applies fixes.

## Testing

```sh
cargo +nightly-2026-06-05 fmt --all -- --check --config-path .infra/style/rustfmt.toml
CARGO_BUILD_JOBS=2 cargo test --locked
CARGO_BUILD_JOBS=2 cargo clippy --all-targets -- -D warnings
```

The integration tests own their CMake fixtures and run real Clang and clang-tidy. Their controlled verify process builds the fixture with real CMake and emits the agreed context protocol; it isolates verify implementation from this crate. Timeout, crash and malformed output tests use controlled clang-tidy substitutes. Cross-tool acceptance additionally runs the actual verify executable.

Generate local Rust API documentation with `cargo doc --no-deps`; the executable's supported interface is also available through `cpp-infra-analyze --help` and `cpp-infra-analyze check --help`.

## License

Licensed under [Apache License 2.0](LICENSE).

## Contributing

Use self-contained fixtures for changes and run the testing commands above. Report reproducible issues to the [project repository](https://github.com/qubit-ltd/cpp-infra-analyze).

## Author

Haixing Hu.

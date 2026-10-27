# cpp-infra-analyze

[![Rust 1.94](https://img.shields.io/badge/Rust-1.94-orange)](Cargo.toml) [![Edition 2024](https://img.shields.io/badge/edition-2024-blue)](Cargo.toml) [![Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE) [![Clang 18](https://img.shields.io/badge/Clang-18-green)](conf/policy/clang-tidy.yaml) [![Protocol 1](https://img.shields.io/badge/protocol-1-blue)](src/reporting.rs) [![English](https://img.shields.io/badge/language-English-red)](README.md)

C++ 库成功编译后，公共头文件中的空指针解引用仍可能没有被发现。`cpp-infra-analyze` 通过 `cpp-infra-verify` 准备分析构建，再用 clang-tidy 18 执行强制 Clang analyzer 与 bugprone 检查。诊断默认使门禁失败，只有公共策略允许的精确 warning 例外可以豁免。

## 环境与配置

首期支持 Linux、Clang/clang-tidy 18、libclang 18 动态运行库、CMake 和单配置 Ninja。构建 Rust crate 不需要 Clang SDK 头文件，也不在链接时依赖 Clang 库。请使用同一 cpp-infra 工具集；`CPP_INFRA_BIN_DIR` 选择其中的工具，该变量存在时缺少 verify 会直接失败，不回退到 PATH。独立开发构建使用 `development` bundle；构建时未设置 `CPP_INFRA_BUILD_REVISION`，`--version --json` 的 revision 为 null。

```sh
cargo build --locked --release
cpp-infra-analyze --version --json
cpp-infra-analyze doctor --json
```

`doctor` 会报告实际加载的 libclang 路径与版本。可用 `CPP_INFRA_LIBCLANG` 显式指定动态库；指定文件缺失时不会回退。未指定时按 LLVM 18 安装位置及系统 soname 发现，始终要求 18.x。缺少运行库返回基础设施错误 `3`，版本不兼容返回配置错误 `2`。实际检查必须加载该运行库，不回退到自写 C++ 词法扫描器。

目标项目自行维护 `.infra/project.toml`、CMake presets 和 `.infra/analyze/config.toml`。analysis profile 必须使用 Clang 18 与 Ninja，关闭 Unity build，并启用 `CMAKE_EXPORT_COMPILE_COMMANDS`。更新器通过 `conf/manifest.json` 交付两份受管理公共策略。`conf/examples/` 仅提供项目所有的配置示例，不纳入受管理文件清单。

```toml
schema = 1
profile = "analysis"
targets = ["example", "example-tests"]
include_tests = true
jobs = 4
timeout_per_translation_unit_seconds = 180
additive_checks = []
```

## 执行分析

```sh
cpp-infra-analyze --project . plan --profile analysis
cpp-infra-analyze --project . --report build/analyze-report.json check --profile analysis --sarif build/analysis.sarif
```

`plan` 输出构建准备和运行期发现节点，不启动外部进程。`check` 调用 `verify prepare --profile NAME --output BUILD_CONTEXT_JSON`，验证输入摘要、目标模型、编译器、生成器、管理标记与构建状态，再持有构建目录锁消费产物。生成头文件先完成构建。`--offline` 传给 verify；`--run-context` 严格校验项目、源码/配置摘要与 profile，若携带运行快照，还校验快照 SHA-256、bundle 和当前可执行文件身份，并保留引用传给 verify。

快照声明的二进制和当前运行的程序都必须与记录的 SHA-256 一致，允许内容相同但路径不同的副本。项目内位于 `build/`、`target/` 之外的报告必须是非输入 JSON 报告；其精确相对路径会写入 `source_excluded_paths` 并传给 verify，仅从源码摘要排除对应文件，配置摘要不受影响。源码、配置目录和已有未知业务文件不能作为输出位置，通过项目外符号链接指回这些目录也会被拒绝。

编排器也可以在 run-context 的 `build_context` 中传入公共报告目录内的绝对路径。分析工具先在消费锁外请求 verify 权威认证旧上下文，合法复用不会重新配置或构建；verify 复验实际 CMake 配置输入、依赖证据和缓存摘要。随后分析工具持锁复核各文件摘要及构建状态，分析结束时再次检查。缺少配置证据的上下文会被拒绝。每次调用的产物写入公共目录下独立的 `analyze/<unique-call-id>/`，并发任务的日志和上下文互不覆盖。检查失败后可读取 JSON 的 `diagnostics`，通过 SARIF 的相对位置定位问题；修正源码后重新执行 `check`，新的源码摘要会触发完整分析。

每个选中的实现源码都需要真实编译命令。同一源码的不同宏等编译上下文分别保留。编译器计算每个 TU 的依赖闭包；source root 下没有选中包含者的公共头文件，会根据所选目标真实编译要求派生独立 include TU，无法建立合法上下文时明确失败。模板仅分析实际实例化路径，不使用仅根据源文件摘要的分析缓存。

被分析的 C/C++ 源码、公共头文件和包含依赖必须可读取且使用 UTF-8；不支持的编码返回配置错误 `2` 并指出文件路径，读取失败不会跳过策略检查。未选中的二进制资源不受影响。NOLINT 分类通过稳定的 libclang 词法 C API 完成，辅助进程支持超时与取消，原生 spelling range 保留续行和原始字符串的物理字节偏移。原始词法 token 不能认证全部预处理头名上下文；遇到无法安全分类的候选会明确返回错误 `2`，不会当作没有指令。

每个 TU 使用独立编译数据库与 YAML 诊断导出。`jobs` 限制并发数量，超时和取消会终止子进程组。JSON 诊断包含相对路径、严重级别、规则、行列、分析 notes 和来源上下文；只有编译要求相同的重复头文件诊断才合并。第三方 warning 在规范化路径归属后过滤，编译错误仍使检查失败。原始日志与导出诊断保留在 `build/infra-runs/<run-id>/`，制品记录包含 SHA-256。SARIF 2.1.0 使用项目相对 URI 和稳定指纹。

项目内 SARIF 输出必须位于 `build/` 或 `target/`，也可写到项目外。CMake、Conan、README 等保留输入及配置路径会在写入前拒绝，规范化后指向这些位置的别名也适用；已有未知业务文件不能覆盖。

## 策略与例外

项目 `additive_checks` 只能增加 clang-tidy 已知检查，拒绝负向模式与任意 CLI 注入。公共配置之外仍会显式启用强制规则。TOML 配置拒绝未知字段、不支持的 schema、重复目标、重叠或越界的根目录。

配置、策略和例外文件必须位于共同摘要可跟踪的位置，读取前和检查新鲜度时都会核验声明路径及规范化目标。根 `.git`、`build`、`target` 和三类基础设施事务目录不能提供策略输入；运行上下文及已验证构建产物继续使用各自的制品校验协议。

只有公共策略 `exception_rules` 中允许的 warning 才能豁免；将 `exceptions` 设置为以下项目相对 TOML 文件：

```toml
schema = 1
[[exceptions]]
rule = "bugprone-unused-return-value"
path = "src/example.cpp"
reason = "调用方有意忽略返回值，已在 ISSUE-123 审阅。"
# message_digest = "<精确诊断文本的 SHA-256>"
```

例外精确匹配规则与相对路径，可附加精确消息摘要。豁免的诊断仍保留在 JSON 和 SARIF 中，错误、崩溃、超时和配置错误不能豁免。默认公共策略拒绝受治理源码注释中的 NOLINT；允许保留时，使用原生注释范围建立 VFS overlay 中和抑制作用，不修改源码。依赖注释也会中和以收集完整结果，但第三方 warning 仍按归属过滤。

clang-tidy 还可能因原始源码行的字符串或标识符包含 NOLINT 而抑制诊断。工具会保留这些 token，避免改变程序语义；若后端统计仍报告 NOLINT 抑制，会因无法认证结果完整性和诊断归属而返回错误 `2`，不伪造项目自有诊断。没有实际被抑制诊断的字符串和标识符可以正常通过。

退出码：`0` 通过，`1` 门禁/构建失败，`2` 配置/协议/平台错误，`3` 文件系统/进程启动/无法解析的工具失败，`130` 取消。成功和失败都会尽力输出报告，check 不自动修复源码。

## 测试

```sh
cargo +nightly-2026-06-05 fmt --all -- --check --config-path .infra/style/rustfmt.toml
CARGO_BUILD_JOBS=2 cargo test --locked
CARGO_BUILD_JOBS=2 cargo clippy --all-targets -- -D warnings
```

集成测试使用自有 CMake fixture，实际执行 Clang 与 clang-tidy。受控 verify 进程真实构建 fixture，并输出约定构建上下文，使当前 crate 测试不依赖 verify 的实现进度。超时、崩溃和畸形输出用受控 clang-tidy 替身；跨工具验收另行运行真实 verify 可执行文件。

运行 `cargo doc --no-deps` 可生成本地 Rust API 文档；命令行接口也可通过 `cpp-infra-analyze --help` 和 `cpp-infra-analyze check --help` 查看。

## 许可证

采用 [Apache License 2.0](LICENSE) 许可证。

## 贡献

修改时请使用独立 fixture，并运行上述测试命令。可复现的问题请反馈到[项目仓库](https://github.com/qubit-ltd/cpp-infra-analyze)。

## 作者

胡海星（Haixing Hu）。

# Code quality and toolchain policy

**Status:** Gate A1 Rust, dependency, and Qt checks are wired into CI. See [Gate A1 results](research/GATE_A1_RESULTS.md) for locally executed checks. Adopt later tiers alongside [PLAN_SKYBER.md](PLAN_SKYBER.md).

**Owner:** Skyber contributors. **Last reviewed:** 2026-11-21.

## Goals

1. Correct, responsive, maintainable Rust core with safe boundaries to Kyber, C/C++ and Qt.
2. Quick feedback for ordinary pull requests; resource-intensive multimedia validation only when relevant.
3. Native desktop behavior tested as rigorously as networking, especially the floating, frameless webcam.
4. Reproducible builds and dependency/license visibility. Do not describe tests or security features as implemented until verified.
5. No substitution of WebRTC for Kyber. The project uses **Kyber from kyber.tech**, not CRYSTALS-Kyber.

## Approved initial stack

| Scope | Tool | Frequency | Stage |
| --- | --- | --- | --- |
| Rust format | `rustfmt` (`cargo fmt`) | Every PR | A1 |
| Rust static analysis | `cargo clippy`, warnings as errors on owned code | Every PR | A1 |
| Rust compilation | `cargo check` and build matrix Windows/Linux | Every PR | A1 |
| Rust tests | `cargo-nextest`; separate doctests via `cargo test --doc` | Every PR | A1 |
| Qt/QML style and analysis | `qmlformat`, `qmllint`, CMake-generated `all_qmllint` | QML changes / relevant PRs | A1 |
| Dependency audit | `cargo-audit` | Scheduled and dependency changes; consider nonblocking advisory triage | A1 |
| Dependency policy | `cargo-deny` | Every PR after license allowlist is reviewed | A1 |
| Qt behavior | Qt Quick Test, optional C++ Qt Test/`QSignalSpy` | GUI changes | B |
| Rust coverage | `cargo-llvm-cov` with nextest | Nightly and before release | B |
| State/property tests | `proptest`; `loom` for select concurrency models | Relevant tests / deep CI | B+ |
| Native boundary checks | Clang/GCC AddressSanitizer and UndefinedBehaviorSanitizer where supported; `Miri` for compatible pure-Rust code | Scheduled/manual and boundary changes | B+ |
| Fuzzing | `cargo-fuzz` / libFuzzer on parsers and protocol state | Scheduled/manual | A2+ |
| Benchmarks | `Criterion.rs` for microbenchmarks; custom end-to-end media metrics | Baseline + relevant changes | A2+ |
| C++ wrappers when present | `clang-format`, `clang-tidy`, compiler warnings | Changed native C++ code | As needed |
| Dependency upkeep | Dependabot **or** Renovate, not both for same manifests | Scheduled PRs | After manifests exist |

*Qt/QML tooling can require a configured Qt module and import paths; do not apply bare `qmllint` in a way that creates false import errors. Clippy all-feature checks must respect OS-specific feature constraints. Rust coverage is not equivalent to coverage of FFmpeg, VLC, C/C++, or QML.*

## Merge gates

For each code PR, require (once the project scaffolding exists):

- Formatting and static analysis pass for **changed languages**.
- Rust workspace compiles and unit/mock integration tests pass on the targeted platform matrix.
- QML changes pass `qmllint` and affected Qt Quick Tests when those targets exist.
- Behavioral regressions have automated tests; otherwise include a precise reason why not and a manual acceptance recipe.
- Public API changes update documentation; media/protocol changes update technical assumptions and relevant integration tests.
- Dependency and new-license changes are reviewed, including **native SDK, C/C++ libraries, Qt modules and codecs**, which `cargo-deny` alone does not cover.
- No uncontrolled network capture, unbounded queue, secret-bearing logs or misleading E2EE/latency claim.
- CI should fail visibly rather than silently skip a necessary check for missing prerequisites.

For documentation-only PRs, execute link/Markdown checks once configured; do **not** start the multimedia or GPU lanes.

Avoid a single global coverage threshold that hides risk in FFI. **Provisional target:** 85% line coverage for testable, application-owned Rust core crates; report branch coverage and adapter/QML coverage separately when possible. Never use coverage as a substitute for boundary, failure-path and concurrency tests.

## CI tiers and selective triggers

| Tier | Trigger | Expected contents | Does **not** need |
| --- | --- | --- | --- |
| Fast | Every code PR | Format, lint, build, Rust tests, QML tests if changed, policy checks | Physical devices, real media |
| Deep | Scheduled, manual, or relevant high-risk changes | Coverage, longer property tests, fuzz seeds, sanitizers, protocol/FFI stress | Every PR |
| A1 synthetic media | Kyber adapter/transport changes, manual and pre-merge high-risk gate | Two isolated local peers over loopback; synthetic A/V; concurrent tracks and teardown | Two machines or webcam availability |
| A2 physical LAN | Manual, milestone, release | Windows 11 ↔ supported Linux, physical camera/mic/speakers, real LAN, GPU and screen capture, latency | Hosted CI runner |
| Release | Tagged candidate | All relevant tiers, licensing review, release smoke tests, packages/signatures, cross-platform checks | Unsupported Windows 7 targets |

**Trigger selection must be dependency-aware:** changing `skyber-core` may affect media tests even if adapter files did not change. Changed-file filters only reduce unnecessary tests; they may never override a required integration check. Allow a manual workflow dispatch to run any lane. Keep known-flaky tests visible, quarantine with owner and issue rather than treating failures as success.

## Commands to install/configure in future scaffolding

These are **target commands after Cargo, CMake and Qt targets exist**, not evidence they work today:

~~~bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo test --workspace --doc
cargo deny check
cargo audit
cargo llvm-cov nextest --workspace --lcov --output-path coverage.lcov
cmake --build build --target all_qmllint
~~~

Run platform-appropriate feature combinations in CI instead of blindly selecting mutually exclusive backends. Pin Rust toolchain in `rust-toolchain.toml`, use committed `Cargo.lock` for the application, pin supported Qt/CMake/native dependency versions, and reproduce builds in clean environments. Do not add configuration files with fictitious dependency versions or unverified command-line flags.

## Security, compliance and review

- Kyber's published AGPLv3 and Qt's licensing may constrain distribution. Obtain a reviewed dependency/license matrix; Rust-only license checks are incomplete.
- Treat all remote signaling, chat, media metadata, device enumerations and deserialization as untrusted.
- QUIC provides transport encryption, not automatically **end-to-end encryption** if a relay terminates it. Require a documented threat model before claims of E2EE.
- Keep tokens, secrets, contacts, chat and personally identifying telemetry out of tests, screenshots, logs and CI artifacts.
- Use structured `tracing` spans by call/session/stream ID, with no raw media or chat content logged by default.
- Prefer small PRs with ADRs for major architecture decisions. Require explicit reviewer attention for `unsafe`, FFI, concurrency, networking and security changes.

## Rollout

1. **A1:** pin toolchains, workspace rustfmt/Clippy policies, Nextest, mock media, format/lint CI, basic QML checks, audit/licensing intake.
2. **B / MVP:** Qt Quick Test, Rust coverage, explicit FFI safety test suite, visual snapshot checks for remote floating window.
3. **A2 / Internet:** fuzzing of network inputs, real LAN hardware lane, impairment/soak tests, performance regression baselines and release validation.

See [TESTING.md](TESTING.md), [CODING_STYLE.md](CODING_STYLE.md), [AGENTS.md](../AGENTS.md), and [Skyber architecture](PLAN_SKYBER.md).

## References

- Rust book and tools: https://doc.rust-lang.org/cargo/ ; https://doc.rust-lang.org/clippy/
- Nextest and doctests: https://nexte.st/docs/running/
- Coverage: https://nexte.st/docs/integrations/test-coverage/
- Qt QML tooling: https://doc.qt.io/qt-6/qtqml-tooling.html
- Qt QML lint targets: https://doc.qt.io/qt-6/qt-add-qml-module.html
- AGENTS.md convention: https://agents.md/

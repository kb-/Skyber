# Skyber — Agent instructions (AGENT.md)

This file is the **detailed agent playbook**. Root [AGENTS.md](AGENTS.md) exists for automatic discovery by coding agents and points here. Read these before making edits:

1. [docs/PLAN_SKYBER.md](docs/PLAN_SKYBER.md) — product scope, architecture and gates.
2. [docs/QUALITY.md](docs/QUALITY.md) — toolchain, CI and quality policy.
3. [docs/CODING_STYLE.md](docs/CODING_STYLE.md) — Rust, Qt/QML, FFI and documentation conventions.
4. [docs/TESTING.md](docs/TESTING.md) — fast, synthetic, native and hardware test tiers.

## Mission and current state

Skyber is an **ultra-low-latency, one-to-one native calling application** using **Kyber (kyber.tech)** for bidirectional webcam, microphone, and screen sharing, with chat and contacts. It is not the similarly named post-quantum cryptography system. Media must **not** be silently replaced with WebRTC.

**Current stage:** feasibility / Gate A1. At the time this file was written, the repository has *planning documentation only*; verify actual tree and build state at the start of every assignment. Never claim working features, passed tests or built binaries without evidence.

Critical UX behavior: a *separate* borderless, resizable, optionally always-on-top **remote webcam** window, with all controls hidden until hover, no controls taking space away from video, correct aspect ratio, and a remote webcam remaining visible alongside screen sharing.

## Before implementing

1. Inspect relevant code, upstream SDK APIs and licenses; establish the current HEAD and branch. Pin or document exact Kyber SDK revision.
2. Identify the smallest deliverable and affected architectural boundaries; propose an ADR for a material stack/protocol/API change.
3. Do not assume symmetric QUIC streams, direct GPU texture sharing, built-in AEC or internet P2P/NAT traversal are available in Kyber. Validate using a minimal proof first.
4. **Gate A1:** two isolated processes on one Windows 11 machine over loopback, with synthetic sources where device sharing is problematic. **Gate A2:** Windows 11 ↔ Linux on two physical machines over LAN; Windows 7 is not a target.
5. Keep dependencies explicit and pinned when appropriate. Do not add unreviewed licenses, required external services, telemetry or proprietary SDKs.

## Implementation constraints

- Core Rust; native Qt 6/Qt Quick UI; small, carefully reviewed Rust–C++ bridge; prefer direct native video surfaces over copies through a webview.
- Respect module boundaries: core state, Kyber adapter, connectivity, signaling, storage, desktop bridge, QML. No heavy business logic in QML.
- Strongly typed public interfaces, error propagation and structured, privacy-conscious logging. No silent fallback implementations or skipped checks when required tools/dependencies are missing.
- Avoid `unwrap`/`expect` in remote/device/state-handling paths; keep `unsafe` isolated with explicit SAFETY invariants and dedicated tests.
- Network events, device changes, streams and UI callbacks must handle cancellation, reconnection, closing and resource cleanup. Keep media queues bounded and avoid blocking UI/async threads.
- No new remote keyboard/mouse control feature without explicit authorization and design.
- English-only agent-authored source/comments/docs/PR content unless user explicitly asks for another language.
- When editing a plan or established API, preserve previously agreed features and decisions. Report conflicts or departures explicitly rather than quietly dropping them.

## Tests: run only what's relevant, but do not skip a required gate

Once the corresponding project scaffolding exists, typical **fast** commands are:

~~~bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo test --workspace --doc
~~~

After appropriate configs and native build targets exist: `cargo deny check`, `cargo audit`, QML lint/test commands, and targeted coverage. Use the target-platform feature set; don't run unsupported `--all-features` combinations.

**Do not** run full fuzzing, GPU encode/stream tests, two-machine LAN tests, live devices or large native sanitizer suites for ordinary chat/GUI/docs changes. Run relevant targeted synthetic-media tests for Kyber or session changes, and request/manual-trigger hardware A2 for physical-device validation. Never count an unavailable tool as a passing test.

Add automated regression tests when fixing bugs; distinguish implementation tests from visual/hardware acceptance checks. Report exact executed commands and results, plus any skipped checks with an explicit reason.

## Pull request and change discipline

- Prefer small, reviewable feature branches and PRs; write to `main` only when the user requests direct commits.
- Do not overwrite or rewrite unrelated work. Inspect current files immediately before updates; avoid destructive operations.
- Document design choices and test evidence in the PR. Include performance, security, licensing and cross-platform impact when relevant.
- If asked to implement only a plan, do not invent production code or CI configuration merely to make a checklist look complete.
- Before marking work complete: summarize modified files, commits/PRs, implemented vs proposed behavior, and remaining limitations. Do not claim background/async work.

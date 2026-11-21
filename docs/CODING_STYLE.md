# Coding style and engineering conventions

**Status:** Proposed baseline. Apply to new code; document deviations in an ADR and avoid unrelated formatting churn. See [QUALITY.md](QUALITY.md) and [TESTING.md](TESTING.md).

## Global standards

- Prefer clear, small, cohesive modules with explicit ownership. Keep UI, call-state orchestration, Kyber adapter, connectivity, and persistence separated.
- **Rust is the core language; Qt Quick/QML is the UI.** Keep C++ minimal at the boundary; do not make Qt/QML hold network or media-processing logic.
- Prefer composition, encapsulation and typed interfaces to inheritance or global mutable state.
- Add tests for behavior and failure modes; update relevant docs for public changes.
- Fail clearly when a **required dependency is unavailable**. Do not add silent substitutes, alternate backends or optional-import fallbacks unless explicitly part of an agreed compatibility design.
- English for code identifiers, comments, docs, commit messages, and user-facing review text unless localization is requested. Plan UI strings for translation.
- Avoid speculative abstractions: keep an intentionally simple first implementation until measured needs justify complexity.

## Rust

**Naming and organization**
- Use `snake_case` for functions, modules and fields, `UpperCamelCase` for types/traits and `SCREAMING_SNAKE_CASE` for constants.
- Prefer small crates matching architectural boundaries (`skyber-core`, `skyber-media-kyber`, `skyber-connectivity`, `skyber-storage`); keep their public APIs lean.
- Use `rustfmt`, shared workspace lint levels, Clippy warnings as errors in application-owned code. No blanket `#[allow(...)]`; scoped, reasoned allows only.
- Define public types and function signatures clearly; avoid hidden dynamic or stringly-typed protocols. Use enums for call states, track types, negotiated features and errors; avoid magic integers.
- Keep configuration typed and validated; distinguish user preference from negotiated/observed runtime capability.

**Error handling and panics**
- Return `Result<T, E>` for expected failure, with `thiserror` for reusable library errors and `anyhow` with context only at application boundaries.
- Do not discard `Result` or silently retry forever. Backoff must be bounded/cancellable and observable.
- Avoid `unwrap()`/`expect()` for network, device, storage, input or session-control paths. They can be appropriate in tests or proven one-time invariants with an explanation.
- Keep `panic!` out of callbacks and FFI; no unwind across ABI boundaries.

**Async, state, memory, performance**
- Use an explicit **call-session state machine** with verified transitions. Manage cancellation, shutdown, timeouts and join handles deliberately.
- Never block Tokio or the Qt event loop with encoding, decoding, disk I/O or locks. Avoid holding locks across `await`.
- Prefer bounded channels with a documented policy: for live video, dropping stale frames is usually preferable to latency growth; audio/control/chat reliability differ and need separately defined semantics.
- Keep media buffers and ownership explicit. Avoid per-frame allocations/copies and CPU round-trips in GPU paths until measured and justified.
- Prefer `tracing` spans with structured IDs and meaningful errors. Never emit message bodies, media frames, keys, passwords or auth tokens by default.
- Measure before introducing unsafe optimizations.

**Unsafe and FFI**
- All `unsafe` belongs inside a small, reviewed boundary layer where practical; each unsafe block documents the safety invariants (`// SAFETY: ...`).
- Validate pointer lifetimes, nullability, ownership transfer, ABI, alignment, thread affinity and shutdown ordering. Do not expose raw pointers beyond the boundary.
- Build safe Rust wrappers around Kyber/FFmpeg/VLC/C++ operations. Define and test explicit resource-release semantics.

## Qt 6, QML, and native windows

- Run `qmlformat` and `qmllint` using the project's Qt module import configuration.
- QML component filenames: `PascalCase.qml`; `id` and properties: `camelCase`; use descriptive signal names. Keep JavaScript expressions short; no complex business logic embedded in QML.
- Bind to a small Rust-owned view model; Qt objects and scene graph changes must happen on their required threads. Avoid cross-thread direct manipulation of QObjects or graphics resources.
- Floating video window: **frameless, resizable, topmost when enabled, video fills intended client area, controls overlay only on hover/idle**, with no layout shift. Aspect ratio preserved; fit and crop-to-fill are explicit, no stretching.
- Keep controls keyboard-accessible and provide accessible labels even if hidden at rest. Never sacrifice focus or usability for a cosmetic borderless implementation.
- Do not assume cross-platform window flags behave identically: use separate platform adapters and test actual Windows and Linux behavior.
- Use resource loading through Qt modules and avoid unchecked filesystem paths/absolute developer-local paths.

## C++ / native glue (only where required)

- Favor narrow C++20 / Qt 6 interfaces compatible with selected Rust binding (initial candidate: CXX-Qt).
- Use RAII ownership, explicit `const`/references, `clang-format` and `clang-tidy` where configured; compiler warnings must not be ignored.
- Catch native exceptions at controlled C++ boundaries if needed and convert to a typed Rust-facing error or error signal; **never permit exceptions/unwind across FFI**.
- Respect Qt thread affinity, signal lifetime and native GPU resource destruction sequencing.

## Protocols, security and chat

- All peer-originated bytes and message fields are untrusted; validate length, format, authorization and state transitions before applying changes.
- Separate reliable chat/control from latency-sensitive media; version message schemas and define forward/backward compatibility and rejection behavior.
- Authenticate call/session identity and chat peer; document relay trust and key continuity. Do not call QUIC via a terminating relay end-to-end encrypted without an additional verified end-to-end design.
- Default to least privilege for microphone, camera, screen capture, notifications and contact presence; clearly indicate active capture. No remote-control capability implied.
- Store secrets using OS-supported secure storage when available and justified, not plaintext SQLite settings. Keep migrations reversible or recovery-aware as feasible.

## Tests, reviews and documentation

- Test observable behavior rather than private implementation details. Include negative, cancellation and resource-lifecycle paths.
- Prefer deterministic synthetic media and injected time/transport for fast tests; reserve real webcams, microphones, GPU and LAN for explicit hardware suites.
- State in each PR: what changed, why, potential latency/FFI/security impact, tests run and tests not run (with rationale).
- Use rustdoc for public APIs and ADRs for nontrivial architectural choices. Write self-contained comments explaining **why**, not narrating obvious code.
- User-facing UI feature changes should include screenshots or acceptance notes when visuals matter, without revealing private content.

## Small examples

**Prefer explicit, typed session transitions and contextual errors:**

~~~rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Idle,
    Connecting,
    Connected,
    Ending,
}

pub fn begin_call(state: CallState) -> Result<CallState, &'static str> {
    match state {
        CallState::Idle => Ok(CallState::Connecting),
        _ => Err("a call is already active or finishing"),
    }
}
~~~

The example illustrates a typed state transition, **not** a complete production call-session API. Real error types belong in the relevant crate.

## Python developer utilities, if introduced

- Prefer `uv` for isolated tool and script environments; avoid global `pip` assumptions.
- Type-annotate parameters, return types and relevant variables. Use pathlib, clear subprocess return-code handling, and fail on missing required modules instead of masking imports with fallback code.

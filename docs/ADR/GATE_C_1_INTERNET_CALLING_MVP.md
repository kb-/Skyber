# Gate C-1 — IPv4 Internet Calling MVP

**Status:** In progress; C-1a transport proofs and an authenticated signaling/relay service foundation are implemented. C-1c path policy exists as a tested pure state machine, but the desktop call flow is not integrated.
**Scope:** one-to-one IPv4 calls over direct UDP with an authenticated relay fallback. IPv6 remains a full Gate C requirement and is outside this MVP.

## Decisions

- Retain Kyber as the media and encrypted data transport. ICE is used only to discover and nominate a network path.
- Do not assume Quinn can reuse an ICE-selected remote address on a new UDP socket. Quinn must use the nominated ICE socket through `Endpoint::new_with_abstract_socket`, or use a relay tunnel that preserves datagram boundaries.
- Select a path during call setup. If it fails during a call, try bounded reconnect with a new authenticated QUIC connection unless connection migration is demonstrated safe with the pinned stack. Restore call tracks and state explicitly.
- A relay sees encrypted QUIC packets only. Bound packet size, queued packets, bandwidth, session count, idle time and teardown; apply backpressure and close both sides when a peer disconnects.
- Invitations are short-lived, single-use, bound to the intended peer identity and room, and separate from a narrowly scoped reconnect credential. Authenticate both peer identities and bind the room and invitation to the authenticated session. Never log credentials.
- The service operator credential administers the service. It is not required for ordinary users to create invitations. Room creation and invitation acceptance need separate authorization and rate limits.
- The Ubuntu home lab may host signaling and relay on public TCP 443. The current code does not provide a public STUN/TURN service or use UDP 3478; direct Internet ICE gathering and NAT traversal remain unproven. Calling participants do not need inbound port forwards or a VPN for the WSS relay path.
- Complete bounded decoding of peer-controlled Kyber data-packet lengths before enabling Internet media. The two readers identified in the SDK audit currently allocate from an untrusted length.

## Implementation sequence

### C-1a — Transport feasibility

1. Prove ICE nomination and an authenticated pinned-Kyber QUIC handshake over the same selected UDP path.
2. Prove bidirectional pinned-Kyber QUIC over a WebSocket relay using synthetic traffic, including bounded queues, packet limits, loss/backpressure, reconnect and teardown.
3. Do not proceed to the full signaling service until both transport boundaries are viable without a disruptive Kyber fork.

### C-1b — Signaling and invitations

The `skyber-signaling` crate contains an in-memory TLS WebSocket service and server binary. It verifies Ed25519 signatures when an inviter creates a room and when the intended invitee accepts. Invitations expire after two minutes, are single-use, bind the two public identities, reject request nonce replay, and are limited to five per identity per minute. Global caps bound active rooms, pending invites, replay entries and concurrent connections. After acceptance, each peer receives separate random reconnect and relay credentials. A disconnected peer may resume signaling for ten minutes by presenting its reconnect credential and signing a room-bound challenge. Candidate and ICE-credential messages are routed only between the authenticated room members. Connections, WebSocket messages, candidate strings, ICE strings, relay queues, datagrams and per-peer relay bandwidth have fixed limits; idle relay tunnels expire after two minutes. Slow receivers apply bounded backpressure and trigger teardown. Typed secret wrappers redact `Debug` output and zeroize their owned values on drop. Serialized WebSocket frames and TLS/OS buffers are not guaranteed to be zeroized. The service does not log request content.

The `/relay` WSS endpoint authenticates a separate per-peer relay credential and Ed25519 signature, forwards only binary datagrams up to 1200 bytes, and closes the other side when a relay peer disconnects. A local TLS integration test verifies both directions through the server endpoint. The relay carries encrypted QUIC datagrams; it does not terminate or inspect Kyber media.

The service listens on `127.0.0.1:34781` by default. To bind a public address it requires `--internet`, plus a TLS certificate and key. Example: `skyber-signal-server --listen 0.0.0.0:443 --cert server-cert.pem --key server-key.pem --internet`.

C-1b remains incomplete: rooms and credentials are in-memory and disappear on restart; there is no persistent contact/account authorization or operator API/credential; invitations still need a safe client sharing and fingerprint-verification flow; and the authenticated signaling identities are not yet bound to the QUIC transcript. Per-identity limits and global caps do not prevent Sybil identities from consuming service capacity. The server is a controlled feasibility service, not a production public calling service.

### C-1c — Path selection and reconnect

The `path_policy` module now defines tested call setup order (direct UDP, then WSS relay) and a single bounded recovery attempt using a new authenticated QUIC connection on the alternate path. It rejects stale attempt results and does not imply QUIC migration or media track restoration. Integrate this policy into the desktop call controller, then validate direct UDP blocked, all UDP blocked, relay outage, expired/replayed invitations, wrong identity, repeated attempts and one-sided disconnect. Investigate a native UDP relay as an intermediate option when UDP to peers is blocked but UDP to the service works.

### C-1d — Desktop call and protected media

Add Qt call controls and video presentation. Harden the pinned Kyber packet readers before making media reachable from the Internet. Keep business logic in Rust/core and the Qt bridge; QML remains presentation-focused.

### C-1e — Acceptance

Measure call setup, relay throughput, p95 latency, audio continuity, video presentation and resource use. Record platform-specific hardware coverage.

## Acceptance split

| Environment | C-1 expectation |
| --- | --- |
| Windows ↔ Windows with physical devices | Full two-way camera and microphone calling |
| Windows ↔ Ubuntu VM | Authenticated connectivity, synthetic Opus/H.264, decoded playback and Qt video rendering |
| Native Ubuntu with working devices | Full two-way live A/V; deferred until a supported device environment is available |

Gate C-1 completion means a usable IPv4 Internet calling MVP with direct/relay selection and documented limitations. Full Gate C additionally requires IPv6 and the broader supported-topology requirements in `PLAN_SKYBER.md`, or an explicit scope change.

## C-1a evidence

The test `ice_quinn::tests::nominated_ice_pair_carries_pinned_kyber_quic` creates two local ICE agents, exchanges host candidates, nominates a loopback pair, wraps each selected ICE connection as Quinn's UDP socket, and exchanges data over a pinned Kyber bidirectional stream.

The test `ice_quinn::tests::websocket_relay_carries_pinned_kyber_quic` carries a 256 KiB synthetic Kyber stream through a local WebSocket pair. It bounds WebSocket frames and socket queues, delays forwarded frames, drops the first datagram in each direction, and then establishes a new tunnel and QUIC connection after teardown.

The proof uses `webrtc-ice` 0.17.3 (MIT/Apache-2.0), Quinn 0.11.12 and Kyber/Kymux revision `af10dbb3ea016135c0ec8e01143179ad4bda005b`. Quinn's public abstract UDP socket API and Kyber's public `From<quinn::Connection>` conversion are sufficient; no Kyber fork is needed. The `skyber-signaling` server also has local WSS tests for authenticated room signaling and bidirectional relay datagrams.

These are local proofs. They do not establish public STUN/TURN, WSS deployment on TCP 443, Internet NAT behavior, sustained relay performance, desktop path integration, identity binding to the QUIC transcript, hardened Kyber packet decoding, active-call track restoration, or live media quality. The relay transport test's injected delay/drop checks QUIC recovery and bounded adapter behavior; it is not a performance result. The pinned Kyber/Kymux revision still contains two media readers that allocate from peer-controlled lengths; Internet-facing media must remain disabled until that parser risk is fixed and regression-tested.

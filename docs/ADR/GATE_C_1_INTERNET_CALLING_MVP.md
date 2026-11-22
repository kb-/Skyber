# Gate C-1 — IPv4 Internet Calling MVP

**Status:** In progress; local integration composes accepted signaling identities, typed WSS signaling, IPv4 ICE nomination, Quinn over the nominated socket, pre-Kymux room-identity binding and synthetic Kyber streams. The call controller now has a Quinn endpoint connector, typed signaling/ICE socket provider, and authenticated WSS relay fallback. Typed relay clients and selectable per-track Kyber transport modes are also implemented. Desktop calling, public Internet acceptance, and production media remain incomplete.
**Scope:** one-to-one IPv4 calls over direct UDP with an authenticated relay fallback. IPv6 remains a full Gate C requirement and is outside this MVP.

**Media architecture pivot (2026-11-22):** Before implementing the production audio/video path, follow [PLAN_KYBER_MEDIA_REALIGNMENT.md](../PLAN_KYBER_MEDIA_REALIGNMENT.md). Reuse Kyber's optimized media pipeline wherever possible, audit upstream-recommended media modes, and expose supported per-track protocol choices until measurements justify defaults. Preserve existing C-1 transport/signaling work and the documented upstream parser-risk exposure policy.

## Decisions

- Retain Kyber as the media and encrypted data transport. ICE is used only to discover and nominate a network path.
- Do not assume Quinn can reuse an ICE-selected remote address on a new UDP socket. Quinn must use the nominated ICE socket through `Endpoint::new_with_abstract_socket`, or use a relay tunnel that preserves datagram boundaries.
- Select a path during call setup. If it fails during a call, try bounded reconnect with a new authenticated QUIC connection unless connection migration is demonstrated safe with the pinned stack. Restore call tracks and state explicitly.
- A relay sees encrypted QUIC packets only. Bound packet size, queued packets, bandwidth, session count, idle time and teardown; apply backpressure and close both sides when a peer disconnects.
- Invitations are short-lived, single-use, bound to the intended peer identity and room, and separate from a narrowly scoped reconnect credential. Authenticate both peer identities and bind the room and invitation to the authenticated session. Never log credentials.
- The service operator credential administers the service. It is not required for ordinary users to create invitations. Room creation and invitation acceptance need separate authorization and rate limits.
- The Ubuntu home lab may host signaling and relay on public TCP 443. The current code does not provide a public STUN/TURN service or use UDP 3478; direct Internet ICE gathering and NAT traversal remain unproven. Calling participants do not need inbound port forwards or a VPN for the WSS relay path.
- Record the known upstream Kyber/Kymux receive-path allocation risk as an unresolved third-party dependency; Skyber will not fork or patch Kymux for C-1. Continue transport, signaling, Qt, identity-binding, and synthetic-media work. Do not describe the unmodified receive path as safe for arbitrary Internet peers. Share technical vulnerability details with the upstream maintainers through a private report rather than adding a public proof of concept.

## Implementation sequence

### C-1a — Transport feasibility

1. Prove ICE nomination and an authenticated pinned-Kyber QUIC handshake over the same selected UDP path.
2. Prove bidirectional pinned-Kyber QUIC over a WebSocket relay using synthetic traffic, including bounded queues, packet limits, loss/backpressure, reconnect and teardown.
3. Do not proceed to the full signaling service until both transport boundaries are viable without a disruptive Kyber fork.

### C-1b — Signaling and invitations

The `skyber-signaling` crate contains an in-memory TLS WebSocket service and server binary. It verifies Ed25519 signatures when an inviter creates a room and when the intended invitee accepts. Invitations expire after two minutes, are single-use, bind the two public identities, reject request nonce replay, and are limited to five per identity per minute. Global caps bound active rooms, pending invites, replay entries and concurrent connections. After acceptance, each peer receives separate random reconnect and relay credentials. A disconnected peer may resume signaling for ten minutes by presenting its reconnect credential and signing a room-bound challenge. Candidate and ICE-credential messages are routed only between the authenticated room members. Connections, WebSocket messages, candidate strings, ICE strings, relay queues, datagrams and per-peer relay bandwidth have fixed limits; idle relay tunnels expire after two minutes. Slow receivers apply bounded backpressure and trigger teardown. Typed secret wrappers redact `Debug` output and zeroize their owned values on drop. Serialized WebSocket frames and TLS/OS buffers are not guaranteed to be zeroized. The service does not log request content.

The `/relay` WSS endpoint authenticates a separate per-peer relay credential and Ed25519 signature, forwards only binary datagrams up to 1200 bytes, and closes the other side when a relay peer disconnects. A local TLS integration test verifies both directions through the server endpoint. The relay carries encrypted QUIC datagrams; it does not terminate or inspect Kyber media.

The service listens on `127.0.0.1:34781` by default. To bind a public address it requires `--internet`, plus a TLS certificate and key. Example: `skyber-signal-server --listen 0.0.0.0:443 --cert server-cert.pem --key server-key.pem --internet`.

The `skyber-signaling::client::SignalClient` now provides typed invitation creation and acceptance, expected-inviter verification, reconnect requests, bounded ICE candidate/credential sending, an authenticated end-of-candidate event, peer event reception and hangup over a WebSocket supplied by its caller. `RelayClient` joins the separate authenticated `/relay` WebSocket with the peer's relay credential and transfers bounded binary datagrams without merging packet boundaries. A local WSS integration test validates the server certificate against an explicit test root, creates and accepts an identity-bound invitation, routes credentials, candidates and the gathering-complete event, checks secret redaction, forwards datagrams in both directions, and tears the room down. The clients deliberately accept already TLS-validated WebSockets so the desktop can apply the platform trust store or a service certificate pin.

The `skyber-media-kyber::ice_session::IceSession` gathers IPv4 host and optional server-reflexive candidates, reports trickle candidate events, validates remote candidate and ICE-credential lengths, runs controlling/controlled nomination with cancellation and fixed deadlines, and retains the selected ICE agent/socket through `NominatedIceConnection`. The latter exposes the exact selected UDP connection as Quinn's abstract socket and closes ICE resources on explicit close or drop. `negotiate_direct_ice` connects this API to the typed signaling client, verifies the room, expected peer and event order, exchanges credentials/candidates/completion, caps candidate counts and responds to cancellation. The synthetic `authenticated_signaling_drives_direct_ice_for_both_peer_roles` test now exercises WSS invitation and acceptance, both ICE roles, Quinn over the nominated sockets, room authentication over the TLS exporter before Kymux initialization, and a bidirectional synthetic Kyber stream. A separate cancellation regression verifies an in-progress ICE check stops. This remains a local loopback proof. It does not establish public STUN availability or Internet NAT behavior, and it does not integrate direct/relay selection, fallback/reconnect or desktop lifecycle.

C-1b remains incomplete: rooms and credentials are in-memory and disappear on restart; there is no persistent contact/account authorization or operator API/credential; and the desktop does not yet use the signaling WSS connector. `TlsSignalWebSocketConnector` opens signaling WSS either with caller-supplied rustls trust (for an explicit service pin or test root) or the platform native CA store. Its local integration path rejects an untrusted replacement certificate. `InvitationShareCode` provides a bounded, versioned WSS invitation format binding the endpoint, room, one-use ticket, inviter and intended invitee; parsing rejects non-WSS/credential-bearing/query/fragment endpoints and a recipient identity mismatch. The code contains a bearer ticket and callers must only reveal it deliberately. These are typed core APIs, not yet wired to Qt sharing/acceptance, and embedding the inviter key does not establish trust: a client must display its fingerprint and let users verify it over an independent channel. The authenticated room identities are bound to the QUIC TLS exporter by the pre-Kymux exchange exercised in local transport tests. Per-identity limits and global caps do not prevent Sybil identities from consuming service capacity. The server is a controlled feasibility service, not a production public calling service.

### C-1c — Path selection and reconnect

The `path_policy` module defines tested call setup order (direct UDP, then WSS relay) and a single bounded recovery attempt using a new authenticated QUIC connection on the alternate path. It rejects stale attempt results and does not imply QUIC migration or media track restoration. `SignalingPathSocketProvider` acquires direct sockets through `negotiate_direct_ice` and authenticated relay sockets through `TlsRelayWebSocketConnector`, which requires an explicit rustls trust configuration and rejects cleartext or credential-bearing relay URLs. The relay service now reports readiness only after both authenticated peer sockets have joined, preventing the first peer's QUIC handshake packets from being dropped during the join race. `QuinnCallPathConnector` constructs the inviter-client or invitee-server Quinn endpoint over the selected abstract socket and completes the handshake; the caller supplies explicit client/server TLS configurations. `CallController` owns direct-first attempts, one alternate-path reconnect, cancellation, QUIC room authentication before Kymux, signaling lifecycle events, and teardown. A local integration test creates and accepts an invitation, forces direct ICE setup to fail, joins both authenticated WSS relay peers, establishes Quinn, authenticates both room identities, exchanges a synthetic Kyber stream, then establishes a newly authenticated direct-ICE Quinn connection through the bounded reconnect path and verifies the remote hang-up event. Unit tests cover path ordering, exhaustion, cancellation and local-identity rejection. This validates local fallback and explicit reconnect composition, not reaction to a real socket loss, public Internet NAT, relay outage, or production platform certificate provisioning. Next validate blocked direct UDP, relay outage, expired/replayed invitations, wrong identity, repeated attempts and one-sided disconnect. Investigate a native UDP relay as an intermediate option when UDP to peers is blocked but UDP to the service works.

### C-1d — Desktop call and protected media

Add Qt call controls and video presentation. Keep business logic in Rust/core and the Qt bridge; QML remains presentation-focused. The authenticated room-to-QUIC session binding is implemented and tested before Kymux initialization in transport proofs. Before choosing the **production** capture, codec, media-protocol and playback path, complete the upstream-first comparison and reference proof in [the Kyber media realignment plan](../PLAN_KYBER_MEDIA_REALIGNMENT.md). Offer selectable supported per-track media modes (with negotiation and diagnostics), rather than locking the A1/A2 reliable-only prototype into the product. Use synthetic media for repeatable Internet transport tests. Live-media trials with the unmodified upstream parser, if undertaken, require an explicitly authorized, isolated test between known participants, documented risk acceptance, and bounded process resources; authentication and resource limits reduce exposure but do not fix the underlying allocation risk. No unattended public-beta or arbitrary-peer Internet media exposure should be presented as safe while the upstream issue remains unresolved.

### C-1e — Acceptance

Measure call setup, relay throughput, p95 latency, audio continuity, video presentation and resource use. Record platform-specific hardware coverage.

## Acceptance split

| Environment | C-1 expectation |
| --- | --- |
| Windows ↔ Windows with physical devices | Full two-way camera and microphone calling |
| Windows ↔ Ubuntu VM | Authenticated connectivity, synthetic Opus/H.264, decoded playback and Qt video rendering |
| Native Ubuntu with working devices | Full two-way live A/V; deferred until a supported device environment is available |

Gate C-1 completion means a usable IPv4 Internet calling MVP with direct/relay selection and documented limitations. Full Gate C additionally requires IPv6 and the broader supported-topology requirements in `PLAN_SKYBER.md`, or an explicit scope change.

## Upstream media-parser risk decision (2026-11-22)

**Status:** Known, unresolved third-party security dependency; private upstream report pending.

**Owner of a parser fix:** Kyber/Kymux maintainers.

**Skyber decision:** Document and track; do not introduce or maintain an application-specific Kymux patch as part of Gate C-1.

The pinned Kymux receive path is reported by source review to allocate memory based on peer-controlled packet lengths before Skyber can impose application-level limits. A malicious or defective authenticated participant could therefore cause excessive allocation or denial of service.

Neither signaling authorization, a TLS/QUIC connection, WebSocket datagram-size limits, nor checks after packet decoding constitute a fix.

This ADR intentionally omits source-level triggering details and any exploit reproduction steps; these belong in a private coordinated disclosure to the maintainers.

**Development may continue:** Complete pre-media, two-sided room-identity authentication bound to the QUIC session; direct/relay transport; reconnect, signaling, call lifecycle, synthetic-media validation, and Qt integration.

Do not gate these independent workstreams on an upstream patch. Keep all media activation behind an explicit policy decision rather than accidentally exposing Kyber's unmodified parser through the new public call path.

**Exposure policy:** Synthetic/raw-Quinn transport tests may use the public lab endpoint, but Internet-connected Kyber media with arbitrary or unvetted peers is not approved.

If an engineer intentionally runs a closed live-media experiment with a known, cooperating peer, document the participants, scope, process isolation/resource limits, operator acceptance of the residual denial-of-service risk, and teardown; do not treat that experiment as evidence of production safety.

Peer authentication reduces who can reach the parser; it does not remove the defect.

**Dependency follow-up:** Privately notify the Kyber maintainers through an appropriate security-reporting channel with the pinned revision, affected receive paths, impact assessment and a minimal non-destructive reproduction if one is established.

Track acknowledgement, advisory/fix status and the updated safe upstream pin before revisiting unrestricted Internet-media exposure.

No vulnerability fix, exploitability proof or vendor notification is claimed by this ADR.

**Acceptance consequence:** Gate C-1 transport, signaling and desktop integration can progress and be demonstrated with synthetic traffic.

Do not mark general-public live-media readiness as fully accepted while this risk remains open; record any narrowly controlled trial and the explicit limitation in release documentation.

## C-1a evidence

The test `ice_quinn::tests::nominated_ice_pair_carries_pinned_kyber_quic` creates two local ICE agents, exchanges host candidates, nominates a loopback pair, wraps each selected ICE connection as Quinn's UDP socket, and exchanges data over a pinned Kyber bidirectional stream.

The test `ice_quinn::tests::websocket_relay_carries_pinned_kyber_quic` carries a 256 KiB synthetic Kyber stream through a local WebSocket pair. It bounds WebSocket frames and socket queues, delays forwarded frames, drops the first datagram in each direction, and then establishes a new tunnel and QUIC connection after teardown.

The proof uses `webrtc-ice` 0.17.3 (MIT/Apache-2.0), Quinn 0.11.12 and Kyber/Kymux revision `af10dbb3ea016135c0ec8e01143179ad4bda005b`. Quinn's public abstract UDP socket API and Kyber's public `From<quinn::Connection>` conversion are sufficient; no Kyber fork is needed. The `skyber-signaling` server also has local WSS tests for authenticated room signaling and bidirectional relay datagrams.

These are local proofs. They do not establish public STUN/TURN, WSS deployment on TCP 443, Internet NAT behavior, sustained relay performance, desktop path integration, hardened Kyber packet decoding, active-call track restoration, or live media quality. The room-auth tests bind both peer identities, room, role and TLS exporter before Kymux setup for each new QUIC connection, including a reconnect proof. The relay transport test's injected delay/drop checks QUIC recovery and bounded adapter behavior; it is not a performance result. The unresolved upstream parser risk remains subject to the documented media-exposure restrictions and private vendor disclosure plan above.

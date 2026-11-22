# Gate C-0 — authenticated direct connectivity probe

**Status:** Probe implemented; local IPv4 authentication checks pass. IPv6 on
the current Windows host and any direct Internet path remain unverified.
**Kyber revision:** `kymux` `af10dbb3ea016135c0ec8e01143179ad4bda005b`.

## Scope and security boundary

The `gate_c0` binary proves that one peer can establish a Kyber Quinn
connection, authenticate the client, and exchange a small synthetic request
and response. It does not carry audio, video, chat, or production call data.
The endpoint uses Kyber's raw bidirectional stream API. It does not construct a
`kyproto::Connection`, media endpoint, or `DataPacket`, because the pinned
`DataPacket` reader accepts a peer-supplied length that may drive allocation.

The client verifies the server with an out-of-band SHA-256 certificate pin.
The server verifies a 32-byte token generated for the run; comparison uses
constant-time equality. Tokens are raw bytes in a create-new file, are never
printed, and are cleared from the short-lived in-memory token buffer when it is
dropped. On Unix the generated file mode is `0600`; on Windows it inherits the
parent directory's access control. Exchange records are fixed at 64 bytes,
have a version, sequence, bounded payload, and validated zero padding. The
listener handles one connection at a time and exits after one attempt.

Connect, authentication, exchange, and shutdown have explicit deadlines. The
listener can wait up to 15 minutes for its one peer so the operator has time to
start the remote client. Timeouts close the listener/session. Logs contain no
token, certificate pin, or socket address.

## Local use

Build with the repository's pinned Kyber revision:

```powershell
cargo build -p skyber-media-kyber --bin gate_c0
cargo run -p skyber-media-kyber --bin gate_c0 -- token-create .\gate-c0-token.bin
```

Create a short-lived self-signed test certificate and obtain its DER SHA-256
pin with OpenSSL. Transfer the raw token file to the other peer through a
separate secure channel; do not paste its bytes into a shell command or log.
For example, on Windows:

```powershell
openssl req -x509 -newkey rsa:2048 -keyout server-key.pem -out server-cert.pem -days 2 -nodes -subj /CN=skyber-gate-c0
openssl x509 -in server-cert.pem -outform DER -out server-cert.der
(Get-FileHash .\server-cert.der -Algorithm SHA256).Hash
```

On Ubuntu, obtain the same lowercase hexadecimal pin with
`openssl x509 -in server-cert.pem -outform DER | sha256sum`.
Run the server first, then the client:

```text
gate_c0 server --cert server-cert.pem --key server-key.pem --token-file gate-c0-token.bin
gate_c0 client --pin <64-hex-character-certificate-pin> --token-file gate-c0-token.bin
```

Both sides default to `127.0.0.1:34780`. Socket addresses accept IPv4
`host:port` and bracketed IPv6 `[host]:port` forms. For a non-loopback address,
pass `--internet` on that side and select the intended interface explicitly.

The synthetic loopback harness creates a disposable certificate and token,
then checks IPv4, certificate-pin rejection, token rejection, and IPv6:

```powershell
cargo build -p skyber-media-kyber --bin gate_c0
py -3.12 scripts/test-gate-c0.py target\debug\gate_c0.exe
```

The default harness includes IPv6 and fails if the current platform cannot
bind that endpoint. For the current Windows-only IPv4 baseline, use
`--skip-ipv6`; it prints an explicit `SKIP` line and does not count as IPv6
validation.

## Direct Internet procedure

Use the existing home lab. Generate a new token for the run and a short-lived
server certificate. On the server peer, bind to its selected LAN interface
with `--internet`. Temporarily forward one UDP port from the router to that
host and allow that port only from the testing client's current public source
address in the host firewall. The client uses the router's public IPv4 address
and port with `--internet`. For IPv6, use the server's selected global IPv6
address and a source-scoped firewall allowance; no router port forward is
needed. Transfer the token and certificate pin out of band. Remove the
temporary forward and firewall allowance immediately after the one-shot probe.

The 4G hotspot is reserved for the final independent-network attempt. Do not
enable a DMZ, forward a media port, or expose a media decoder for this proof.

## Results and remaining gate

On the Windows 11 development host, the disposable-certificate integration
harness completed IPv4 loopback. A wrong certificate pin and a wrong token were
both rejected. Unit tests reject malformed fixed-size messages, invalid
padding, bad pins, and non-loopback addresses without `--internet`.

The IPv6 loopback case could not start the pinned Kyber `CommonServer` on this
Windows host: endpoint creation returned Windows socket error 10022 for an
`[::1]` bind. The host has an IPv6 interface, so this is not evidence that the
OS has IPv6 disabled. Do not replace the specific bind with `[::]` for a
loopback test; that would listen on every IPv6 interface. Recheck on Ubuntu
and investigate a narrow upstream-compatible IPv6 bind path before claiming
IPv6 support.

No public Internet, router-forward, firewall, 4G, NAT, or production-media
test has been run from this implementation. Gate C-0 remains open until
IPv4/IPv6 loopback and the source-scoped direct Internet procedure are recorded
on the intended endpoints. Gate C-1 discovery, NAT traversal, and relay work
remain separate.

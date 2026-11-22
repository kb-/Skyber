"""Run Gate C-0 authenticated synthetic exchanges on IPv4 and IPv6 loopback."""

import argparse
import hashlib
from pathlib import Path
import queue
import shutil
import socket
import ssl
import subprocess
import tempfile
import threading


def free_port(family: int, host: str) -> int:
    with socket.socket(family, socket.SOCK_DGRAM) as sock:
        sock.bind((host, 0))
        return sock.getsockname()[1]


def start_server(peer: Path, address: str, cert: Path, key: Path, token: Path):
    process = subprocess.Popen(
        [str(peer), "server", "--listen", address, "--cert", str(cert),
         "--key", str(key), "--token-file", str(token)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    lines: queue.Queue[str | None] = queue.Queue()

    def read_output() -> None:
        assert process.stdout is not None
        for line in process.stdout:
            lines.put(line.rstrip("\r\n"))
        lines.put(None)

    threading.Thread(target=read_output, daemon=True).start()
    try:
        ready = lines.get(timeout=10)
    except queue.Empty as error:
        process.kill()
        process.wait()
        raise RuntimeError("Gate C-0 listener did not become ready") from error
    if ready != "Gate C-0 listener ready for one probe.":
        if process.poll() is None:
            process.kill()
        process.wait()
        error_output = process.stderr.read() if process.stderr else ""
        raise RuntimeError(
            f"unexpected Gate C-0 listener output: {ready}; "
            f"exit={process.returncode}; error={error_output.strip()}"
        )
    return process


def run_case(peer: Path, cert: Path, key: Path, server_token: Path,
             client_token: Path, pin: str,
             address: str, expected_success: bool, label: str) -> None:
    server = start_server(peer, address, cert, key, server_token)
    try:
        client = subprocess.run(
            [str(peer), "client", "--connect", address, "--pin", pin,
             "--token-file", str(client_token)],
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        server_code = server.wait(timeout=20)
        server_err = server.stderr.read() if server.stderr else ""
        if expected_success:
            if client.returncode != 0 or server_code != 0:
                raise RuntimeError(
                    f"{label} failed: client={client.stderr.strip()} "
                    f"server={server_err.strip()}"
                )
            if "authenticated exchange completed" not in client.stdout:
                raise RuntimeError(f"{label}: client omitted completion result")
        elif client.returncode == 0 or server_code == 0:
            raise RuntimeError(f"{label} unexpectedly authenticated")
        elif "certificate pin" in label and pin.lower() in client.stderr.lower():
            raise RuntimeError(f"{label} exposed the certificate pin in an error message")
        print(f"PASS {label}")
    except Exception:
        if server.poll() is None:
            server.kill()
            server.wait()
        raise


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("peer", type=Path, help="built gate_c0 executable")
    parser.add_argument(
        "--skip-ipv6",
        action="store_true",
        help="skip IPv6 only when the platform limitation is reported separately",
    )
    args = parser.parse_args()
    peer = args.peer.resolve(strict=True)
    openssl = shutil.which("openssl")
    if openssl is None:
        raise RuntimeError("OpenSSL is required to create a temporary loopback test certificate")

    unsafe_bind = subprocess.run(
        [str(peer), "server", "--listen", "0.0.0.0:34780", "--cert", "unused",
         "--key", "unused", "--token-file", "unused"],
        capture_output=True,
        text=True,
        timeout=5,
        check=False,
    )
    if unsafe_bind.returncode == 0 or "require explicit --internet mode" not in unsafe_bind.stderr:
        raise RuntimeError("non-loopback listener was not gated by --internet")
    print("PASS non-loopback listener requires explicit --internet")

    with tempfile.TemporaryDirectory(prefix="skyber-gate-c0-") as temporary:
        root = Path(temporary)
        cert = root / "test-cert.pem"
        key = root / "test-key.pem"
        token = root / "token.bin"
        wrong_token = root / "wrong-token.bin"
        subprocess.run(
            [openssl, "req", "-x509", "-newkey", "rsa:2048", "-keyout", str(key),
             "-out", str(cert), "-days", "1", "-nodes", "-subj", "/CN=skyber-gate-c0"],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        subprocess.run([str(peer), "token-create", str(token)], check=True,
                       capture_output=True, text=True)
        secret = token.read_bytes()
        if len(secret) != 32:
            raise RuntimeError("Gate C-0 token command returned an invalid token file")
        wrong_token.write_bytes(bytes(byte ^ 0xFF for byte in secret))
        pin = hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.read_text(encoding="ascii"))).hexdigest()
        wrong_pin = ("0" if pin[0] != "0" else "1") + pin[1:]

        run_case(peer, cert, key, token, token, pin,
                 f"127.0.0.1:{free_port(socket.AF_INET, '127.0.0.1')}", True,
                 "IPv4 loopback exchange")
        run_case(peer, cert, key, token, token, wrong_pin,
                 f"127.0.0.1:{free_port(socket.AF_INET, '127.0.0.1')}", False,
                 "wrong certificate pin rejected")
        run_case(peer, cert, key, token, wrong_token, pin,
                 f"127.0.0.1:{free_port(socket.AF_INET, '127.0.0.1')}", False,
                 "wrong token rejected")
        if args.skip_ipv6:
            print("SKIP IPv6 loopback: explicitly skipped; this does not pass IPv6 validation")
        else:
            run_case(peer, cert, key, token, token, pin,
                     f"[::1]:{free_port(socket.AF_INET6, '::1')}", True,
                     "IPv6 loopback exchange")


if __name__ == "__main__":
    main()

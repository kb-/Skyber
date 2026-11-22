"""Prepare, run and verify the opt-in Gate A2 synthetic media suite."""

import argparse
from datetime import datetime, timezone
import hashlib
import ipaddress
import json
from pathlib import Path
import platform
import runpy
import secrets
import shutil
import subprocess
import tempfile
import time

A1 = runpy.run_path(str(Path(__file__).with_name("test-a1.py")))
ROLES = ("listener", "initiator")
CASES = {
    "av-listener": ("av", "listener", 2000, None),
    "av-initiator": ("av", "initiator", 2000, None),
    "concurrent-screen-stop": ("av_with_screen", "both", 8000, 6900),
    "wrong-token": ("audio", "both", 5800, None),
    "wrong-fingerprint": ("audio", "both", 5800, None),
    "rejoin-same-port": ("av", "listener", 1000, None),
}


def save(path: Path, data: dict) -> None:
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")


def prepare(address: str, port: int) -> None:
    ip = ipaddress.IPv4Address(address)
    if not any(ip in ipaddress.IPv4Network(net) for net in
               ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")) or not 1 <= port <= 65535:
        raise ValueError("use a private IPv4 address and a nonzero UDP port")
    run_id = stamp()
    root = Path(tempfile.mkdtemp(prefix="skyber-a2-"))
    listener = root / "listener"
    listener.mkdir()
    cert, key = listener / "cert.pem", listener / "key.pem"
    subprocess.run([
        "openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt",
        "ec_paramgen_curve:prime256v1", "-nodes", "-subj", "/CN=localhost",
        "-addext", f"subjectAltName=IP:{ip},DNS:localhost", "-days", "1",
        "-out", str(cert), "-keyout", str(key),
    ], check=True, capture_output=True)
    der = subprocess.check_output(["openssl", "x509", "-in", str(cert), "-outform", "DER"])
    pin = hashlib.sha256(der).hexdigest()
    for role in ROLES:
        for scenario, (profile, direction, duration, stop) in CASES.items():
            case = root / role / scenario
            case.mkdir(parents=True)
            if role == "listener":
                shutil.copyfile(cert, case / "cert.pem")
                shutil.copyfile(key, case / "key.pem")
            else:
                token = load(listener / scenario / "peer.json")["secret"]
            if role == "listener" or scenario == "wrong-token":
                token = secrets.token_hex(24)
            save(case / "peer.json", {
                "role": role, "port": port, "profile": profile, "direction": direction,
                "duration_ms": duration, "screen_stop_ms": stop, "secret": token,
                "fingerprint": "00" * 32 if role == "initiator" and scenario == "wrong-fingerprint" else pin,
                ("bind_address" if role == "listener" else "remote_address"): str(ip),
            })
            save(case / "run.json", {"run_id": run_id, "scenario": scenario, "role": role})
    cert.unlink()
    key.unlink()
    print(f"{run_id} {root}")


def run(peer: Path, case: Path, output: Path) -> None:
    metadata = load(case / "run.json")
    role, scenario, run_id = (metadata[key] for key in ("role", "scenario", "run_id"))
    if role not in ROLES or scenario not in CASES or not run_id.replace(".", "").isalnum():
        raise ValueError("invalid run metadata")
    peer = peer.resolve(strict=True)
    result = output / run_id / scenario
    result.mkdir(parents=True, exist_ok=True)
    report = result / f"{role}.json"
    report.unlink(missing_ok=True)
    run_error = None
    started = stamp()
    # Fresh markers for every attempt. Credentials stay in the private source directory.
    with tempfile.TemporaryDirectory(prefix="skyber-a2-peer-") as tmp:
        work = Path(tmp)
        for name in ("peer.json", "cert.pem", "key.pem"):
            if (case / name).is_file():
                shutil.copyfile(case / name, work / name)
        with (result / f"{role}.stdout").open("wb") as stdout, (
            result / f"{role}.stderr"
        ).open("wb") as stderr:
            child = subprocess.Popen([str(peer), "--lan", str(work)], stdout=stdout, stderr=stderr)
            try:
                if role == "listener":
                    A1["await_marker"](work / "ready", child, time.monotonic() + 5)
                    print(f"{run_id} {scenario} listening", flush=True)
                resources = A1["monitor_processes"]({role: child}, 80)
            except (RuntimeError, subprocess.TimeoutExpired) as error:
                run_error = str(error)
            finally:
                A1["stop_process"](child)
            status = child.returncode
            authenticated = (work / "authenticated").is_file()
    inventory = peer.with_name("build-inventory.txt")
    save(report, {
        **metadata, "exit_code": status, "runner_error": run_error, "os": platform.platform(), "arch": platform.machine(),
        "run_started_utc": started, "run_finished_utc": stamp(), "source": "synthetic", "codecs": ["Opus", "H.264"],
        "network": "Wi-Fi", "authenticated": authenticated, "port": load(case / "peer.json")["port"],
        "peer_sha256": hashlib.sha256(peer.read_bytes()).hexdigest(),
        "resource_samples": {
            "clock_domain": "test_runner_monotonic_us_since_launch",
            "processes": resources if run_error is None else {},
        },
        "build_inventory": inventory.read_text(encoding="utf-8") if inventory.exists() else None,
    })
    print(f"{role} exited {status} for {scenario}", flush=True)
    if run_error is not None:
        raise RuntimeError(run_error)


def verify(root: Path) -> bool:
    results = []
    port = None
    for scenario, (profile, direction, _, stop) in CASES.items():
        case = root / scenario
        row = {"scenario": scenario, "profile": profile, "direction": direction, "passed": False}
        try:
            peers = [load(case / f"{role}.json") for role in ROLES]
            row["peers"] = peers
            require = A1["require"]
            for role, peer in zip(ROLES, peers):
                require((peer["run_id"], peer["scenario"], peer["role"]) ==
                        (root.name, scenario, role), "mismatched run metadata")
            require(all(peer["runner_error"] is None for peer in peers), "peer runner failed")
            require(peers[0]["os"].startswith("Windows") and peers[1]["os"].startswith("Linux"),
                    "expected a Windows listener and Linux initiator")
            if port is None:
                port = peers[0]["port"]
            require(all(peer["port"] == port for peer in peers), "UDP port changed")
            if scenario in ("wrong-token", "wrong-fingerprint"):
                require(all(peer["exit_code"] != 0 and not peer["authenticated"] for peer in peers),
                        "expected both peers to exit before track registration")
                role, reason = (
                    ("listener", "authentication rejected") if scenario == "wrong-token"
                    else ("initiator", "Certificate hash mismatch")
                )
                require(reason in A1["read_stderr"](case, role), f"missing diagnostic: {reason}")
            else:
                require(all(peer["exit_code"] == 0 and peer["authenticated"] for peer in peers),
                        "expected both peers to authenticate and finish")
                A1["verify_media"](case, profile, direction, stop is not None, lan=True)
                for role, peer in zip(ROLES, peers):
                    peer["tracks"] = A1["read_result"](case, role)["tracks"]
            row["passed"] = True
        except (OSError, ValueError, RuntimeError, KeyError) as error:
            row["error"] = str(error) or "unexpected peer metadata or exit status"
        results.append(row)
    save(root / "results.json", {"run_id": root.name, "cases": results})
    lines = [f"# A2-0 Wi-Fi results for {root.name}", "", "| Scenario | Result |", "| --- | --- |"]
    for row in results:
        lines.append(f"| {row['scenario']} | {'Pass' if row['passed'] else 'Fail'} |")
    (root / "results.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return all(row["passed"] for row in results)


def check_lan_arguments(peer: Path) -> None:
    config = {
        "role": "listener", "port": 34829, "profile": "audio", "direction": "both",
        "duration_ms": 1000, "secret": "local-config-check", "fingerprint": "00" * 32,
    }
    cases = (
        ([], {"bind_address": "192.168.1.1"}, "invalid role-specific LAN addresses"),
        (["--lan"], {}, "invalid role-specific LAN addresses"),
        (["--lan"], {"remote_address": "192.168.1.1"}, "invalid role-specific LAN addresses"),
        (["--lan"], {"bind_address": "0.0.0.0"}, "LAN requires private IPv4"),
    )
    with tempfile.TemporaryDirectory(prefix="skyber-a2-config-") as tmp:
        work = Path(tmp)
        for flags, fields, reason in cases:
            save(work / "peer.json", config | fields)
            result = subprocess.run([str(peer), *flags, str(work)], capture_output=True,
                                    text=True, timeout=5)
            A1["require"](result.returncode != 0 and reason in result.stderr
                          and not (work / "ready").exists(), f"expected {reason}")
    print("LAN configuration rejection checks passed")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    stage = commands.add_parser("prepare")
    stage.add_argument("--bind-address", required=True)
    stage.add_argument("--port", type=int, required=True)
    launch = commands.add_parser("run")
    launch.add_argument("peer", type=Path)
    launch.add_argument("case", type=Path)
    launch.add_argument("--output", type=Path, default=Path("target/a2/runs"))
    check = commands.add_parser("verify")
    check.add_argument("run", type=Path)
    local = commands.add_parser("local")
    local.add_argument("peer", type=Path)
    local.add_argument("--output", type=Path, default=Path("target/a2/local"))
    args = parser.parse_args()
    if args.command == "prepare":
        prepare(args.bind_address, args.port)
    elif args.command == "run":
        run(args.peer, args.case, args.output)
    elif args.command == "verify":
        raise SystemExit(0 if verify(args.run) else 1)
    else:
        output = args.output / stamp()
        output.mkdir(parents=True)
        peer = args.peer.resolve(strict=True)
        check_lan_arguments(peer)
        A1["run_suite"](peer, output)


if __name__ == "__main__":
    main()

"""Run the consent-based A2-1 live-source loopback check."""

import argparse
from datetime import datetime, timezone
from pathlib import Path
import runpy
import sys


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("peer", type=Path)
    ubuntu = sys.platform == "linux"
    device_type = str if ubuntu else int
    parser.add_argument("--camera", type=device_type, required=True)
    parser.add_argument("--microphone", type=device_type, required=True)
    parser.add_argument("--output", type=Path,
                        default=Path("target/a2-1b/runs" if ubuntu else "target/a2-1a/runs"))
    screen = parser.add_mutually_exclusive_group()
    screen.add_argument("--with-screen", action="store_true")
    screen.add_argument("--cancel-screen", action="store_true")
    args = parser.parse_args()
    if not (ubuntu or sys.platform == "win32"):
        parser.error("this check runs on Windows and Ubuntu")
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    run = args.output / stamp
    run.mkdir(parents=True)
    suite = runpy.run_path(str(Path(__file__).with_name("test-a1.py")))
    print(f"A2-1 capture results directory {run}", flush=True)
    with_screen = args.with_screen or args.cancel_screen
    if args.cancel_screen:
        print("Cancel the screen capture picker without selecting a source.", flush=True)
    elif with_screen:
        print("Select a non-sensitive window in the screen capture picker within 60 seconds.", flush=True)
    suite["run_case"](
        args.peer.resolve(strict=True), run / "live", suite["available_port"](),
        "av_with_screen" if with_screen else "av", "listener",
        duration=10500 if with_screen else 4500,
        screen_stop=4500 if with_screen else None,
        failure="capture_cancelled" if args.cancel_screen else None,
        capture={"camera": args.camera, "microphone": args.microphone, "screen": with_screen},
    )


if __name__ == "__main__":
    main()

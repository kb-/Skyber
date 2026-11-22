"""Play synthetic audio, camera, and screen tracks received through Kyber."""

import argparse
from datetime import datetime, timezone
from pathlib import Path
import runpy
import sys


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("peer", type=Path)
    parser.add_argument("--audio-output", type=int if sys.platform == "win32" else str)
    parser.add_argument("--output", type=Path, default=Path("target/a2-1c/runs"))
    args = parser.parse_args()
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    run = args.output / stamp
    run.mkdir(parents=True)
    suite = runpy.run_path(str(Path(__file__).with_name("test-a1.py")))
    print(f"A2-1c playback results directory {run}", flush=True)
    suite["run_case"](
        args.peer.resolve(strict=True), run / "playback", suite["available_port"](),
        "av_with_screen", "listener", duration=12000, screen_stop=7000,
        playback=True, audio_output=args.audio_output,
    )


if __name__ == "__main__":
    main()

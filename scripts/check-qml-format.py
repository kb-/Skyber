"""Compare tracked QML sources with the installed Qt formatter."""

from pathlib import Path
import subprocess
import sys


def main() -> int:
    if len(sys.argv) < 2:
        raise SystemExit("usage: check-qml-format.py QML_FILE ...")
    changed = False
    for name in sys.argv[1:]:
        path = Path(name)
        result = subprocess.run(
            ["qmlformat", str(path)], check=True, stdout=subprocess.PIPE, text=True, encoding="utf-8"
        )
        if result.stdout != path.read_text(encoding="utf-8"):
            print(f"QML formatting differs in {path}. Run qmlformat -i {path}.", file=sys.stderr)
            changed = True
    return int(changed)


if __name__ == "__main__":
    raise SystemExit(main())

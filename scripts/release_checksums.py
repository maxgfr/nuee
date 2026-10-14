#!/usr/bin/env python3
"""Write dist/SHA256SUMS for the dist/nuee-* release binaries."""

import hashlib
import sys
from pathlib import Path


def main() -> None:
    dist = Path(__file__).resolve().parent.parent / "dist"
    files = sorted(p for p in dist.glob("nuee-*") if p.is_file())
    if not files:
        sys.exit("release_checksums: no dist/nuee-* files")
    lines = []
    for path in files:
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
        lines.append(f"{digest.hexdigest()}  {path.name}\n")
    (dist / "SHA256SUMS").write_text("".join(lines))
    print(f"release_checksums: {len(files)} file(s)")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Set the release version in Cargo.toml ([workspace.package]), Cargo.lock and fuzz/Cargo.lock (nuee* packages)."""

import re
import sys
from pathlib import Path

SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$")


def fail(message: str) -> None:
    sys.exit(f"prepare_release: {message}")


def set_workspace_version(text: str, version: str) -> str:
    section = re.search(r"(?ms)^\[workspace\.package\]\s*$(.*?)(?=^\[|\Z)", text)
    if not section:
        fail("Cargo.toml has no [workspace.package] section")
    body, count = re.subn(
        r'(?m)^(version\s*=\s*)"[^"]*"', rf'\g<1>"{version}"', section.group(1), count=1
    )
    if count != 1:
        fail("no version under [workspace.package]")
    return text[: section.start(1)] + body + text[section.end(1) :]


def set_lock_versions(text: str, version: str) -> str:
    # nuee-fuzz keeps its own version (fuzz/Cargo.toml); it only appears in fuzz/Cargo.lock.
    pattern = re.compile(r'(?m)^(name = "nuee(?!-fuzz")[^"]*"\nversion = )"[^"]*"')
    text, count = pattern.subn(rf'\g<1>"{version}"', text)
    if count == 0:
        fail("no nuee package found in Cargo.lock")
    return text


def main() -> None:
    if len(sys.argv) != 2 or not SEMVER.match(sys.argv[1]):
        fail("usage: prepare_release.py <major.minor.patch>")
    version = sys.argv[1]
    root = Path(__file__).resolve().parent.parent
    manifest = root / "Cargo.toml"
    manifest.write_text(set_workspace_version(manifest.read_text(), version))
    # The fuzz workspace depends on the crates by path, so its lockfile records them too.
    for lock in (root / "Cargo.lock", root / "fuzz" / "Cargo.lock"):
        lock.write_text(set_lock_versions(lock.read_text(), version))
    print(f"prepare_release: version set to {version}")


if __name__ == "__main__":
    main()

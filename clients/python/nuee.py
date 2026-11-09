"""Use a Nuée group from Python: a thin client over the `nuee` command and its --json events.

    from nuee import Group

    group = Group(os.environ["NUEE_GROUP"])
    result = group.build("npm ci && npm run build", keep=["dist"], out="build")
    print(result["passed"], result["log"])

It needs no package, only the `nuee` binary on PATH. The group link travels on stdin, so that it
does not show in the process list.
"""

import json
import subprocess
from typing import Iterable, Optional


class NueeError(Exception):
    """A `nuee` command failed; `events` holds what it reported before failing."""

    def __init__(self, message, code, events):
        super().__init__(message)
        self.code = code
        self.events = events


class Group:
    """One group, named by its link. `key` is a CI key (`nuee group ci-key`), if any."""

    def __init__(self, link: str, binary: str = "nuee", key: Optional[str] = None):
        self.link = link
        self.binary = binary
        self.key = key

    def _run(self, argv, tolerate_failure=False):
        env = None
        if self.key is not None:
            import os

            env = dict(os.environ, NUEE_KEY=self.key)
        done = subprocess.run(
            [self.binary, "--json", *argv],
            input=self.link,
            capture_output=True,
            text=True,
            env=env,
            check=False,
        )
        events = []
        for line in done.stdout.splitlines():
            try:
                events.append(json.loads(line))
            except ValueError:
                continue
        if done.returncode != 0 and not tolerate_failure:
            raise NueeError(done.stderr.strip() or "nuee failed", done.returncode, events)
        return events

    @staticmethod
    def _event(events, name):
        return next((e for e in events if e.get("event") == name), None)

    @staticmethod
    def _options(rev, out, fresh, timeout):
        argv = []
        if rev is not None:
            argv += ["--rev", rev]
        if out is not None:
            argv += ["--out", str(out)]
        if timeout is not None:
            argv += ["--timeout", str(timeout)]
        if fresh:
            argv.append("--fresh")
        return argv

    def status(self) -> dict:
        """The machines of the group and which one coordinates."""
        return self._event(self._run(["group", "status", "-"]), "group")

    def build(
        self,
        command: str,
        keep: Iterable[str] = (),
        out=None,
        rev: Optional[str] = None,
        fresh: bool = False,
        timeout: Optional[int] = None,
    ) -> dict:
        """Builds a pushed commit on the group. A failing build is returned, not raised."""
        argv = ["build", "--on", "-", *self._options(rev, out, fresh, timeout)]
        for path in keep:
            argv += ["--keep", path]
        events = self._run([*argv, "--", command], tolerate_failure=True)
        result = self._event(events, "build")
        if result is None:
            raise NueeError("the build gave no result", 1, events)
        return result

    def shard(
        self,
        shards: int,
        command: str,
        out=None,
        rev: Optional[str] = None,
        fresh: bool = False,
        timeout: Optional[int] = None,
    ) -> list:
        """Cuts `command` ({i}, {n}) across the group. Returns one result per shard."""
        argv = ["shard", str(shards), "--on", "-", *self._options(rev, out, fresh, timeout)]
        events = self._run([*argv, "--", command], tolerate_failure=True)
        results = [e for e in events if e.get("event") == "shard"]
        if len(results) != shards:
            raise NueeError("not every shard gave a result", 1, events)
        return results

    def files(self) -> dict:
        """What the group keeps: `{"files": [...], "sites": [...]}`."""
        return self._event(self._run(["group", "ls", "-"]), "drive")

    def put(self, path, name: Optional[str] = None) -> dict:
        """Gives a file or folder to the group."""
        argv = ["group", "put", "-", str(path)]
        if name is not None:
            argv += ["--name", name]
        return self._event(self._run(argv), "kept")

    def get(self, name: str, out=".", force: bool = False) -> list:
        """Downloads one of the group's files; returns the paths written."""
        argv = ["group", "get", "-", name, "--out", str(out)]
        if force:
            argv.append("--force")
        return [e["path"] for e in self._run(argv) if e.get("event") == "file"]

    def remove(self, name: str) -> None:
        """Deletes one of the group's files."""
        self._run(["group", "rm", "-", name])

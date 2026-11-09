"""The Python client against a stand-in for the `nuee` binary."""

import json
import os
import stat
import tempfile
import unittest
from pathlib import Path

from nuee import Group, NueeError

FAKE = r"""#!/usr/bin/env python3
import json, os, sys
argv = sys.argv[1:]
link = sys.stdin.read()
with open(os.environ["FAKE_LOG"], "a") as f:
    f.write(json.dumps({"argv": argv, "link": link, "key": os.environ.get("NUEE_KEY")}) + "\n")
verb = argv[1]
if verb == "build":
    failing = "bad" in argv[-1]
    print("not json")
    print(json.dumps({"event": "build", "passed": not failing, "exit_code": 2 if failing else 0}))
    sys.exit(1 if failing else 0)
if verb == "shard":
    for i in range(int(argv[2])):
        print(json.dumps({"event": "shard", "name": f"s{i}", "passed": True}))
    sys.exit(0)
if argv[2] == "status":
    print(json.dumps({"event": "group", "name": "lab"}))
    sys.exit(0)
print("the group is unreachable", file=sys.stderr)
sys.exit(4)
"""


class ClientTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        binary = Path(self.dir.name) / "nuee"
        binary.write_text(FAKE)
        binary.chmod(binary.stat().st_mode | stat.S_IEXEC)
        self.log = Path(self.dir.name) / "log"
        os.environ["FAKE_LOG"] = str(self.log)
        self.group = Group("the-link", binary=str(binary), key="k" * 64)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_the_link_goes_on_stdin_and_the_key_in_the_environment(self):
        self.assertEqual(self.group.status()["name"], "lab")
        call = self.calls()[0]
        self.assertEqual(call["link"], "the-link")
        self.assertEqual(call["key"], "k" * 64)
        self.assertNotIn("the-link", call["argv"])

    def test_a_failing_build_is_a_result(self):
        result = self.group.build("make bad", keep=["dist"], fresh=True)
        self.assertFalse(result["passed"])
        argv = self.calls()[0]["argv"]
        self.assertEqual(argv[-2:], ["--", "make bad"])
        self.assertIn("--fresh", argv)
        self.assertEqual(argv[argv.index("--keep") + 1], "dist")

    def test_every_shard_comes_back(self):
        self.assertEqual(len(self.group.shard(3, "t {i}/{n}")), 3)

    def test_a_network_failure_raises(self):
        with self.assertRaises(NueeError) as caught:
            self.group.files()
        self.assertEqual(caught.exception.code, 4)
        self.assertIn("unreachable", str(caught.exception))


if __name__ == "__main__":
    unittest.main()

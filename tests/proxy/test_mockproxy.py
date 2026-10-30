"""Exercise the synthetic Git/probe fixture before spending time on a k3d build."""

import http.server
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("mockproxy", ROOT / "deploy/proxy/ci/mockproxy.py")
MOCK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOCK)


class MockProxyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        MOCK.Proxy.git_root = MOCK.repositories(self.root)
        MOCK.Proxy.git_backend = subprocess.check_output(["git", "--exec-path"], text=True).strip() + "/git-http-backend"
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), MOCK.Proxy)
        self.thread = threading.Thread(target=self.server.serve_forever)
        self.thread.start()
        self.addCleanup(self.stop)
        self.proxy = "http://127.0.0.1:" + str(self.server.server_port)

    def stop(self):
        self.server.shutdown()
        self.thread.join(timeout=5)
        self.server.server_close()

    def test_git_uses_real_smart_http_and_downloads_the_fixture(self):
        environment = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
                       "GIT_TERMINAL_PROMPT": "0", "no_proxy": "", "NO_PROXY": ""}
        target = self.root / "clone"
        result = subprocess.run([
            "git", "-c", "http.proxy=" + self.proxy, "clone", "-q", "--depth=1",
            "http://git.proxy-test.invalid/repo.git", str(target),
        ], env=environment, capture_output=True, text=True, timeout=15, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("phase=dockerfile", (target / "Dockerfile").read_text())
        plan = json.loads((target / "railpack-plan.json").read_text())
        self.assertNotIn("secrets", plan)
        self.assertIn("phase=railpack", plan["steps"][0]["commands"][0]["cmd"])

    def test_fixture_planner_requires_proxy_and_copies_only_plan(self):
        context = self.root / "work"
        output = self.root / "plan.json"
        info = self.root / "info.json"
        environment = {**os.environ, "http_proxy": self.proxy, "HTTP_PROXY": self.proxy,
                       "no_proxy": "", "NO_PROXY": ""}
        result = subprocess.run([
            sys.executable, str(ROOT / "deploy/proxy/ci/railpack"), "prepare", str(context),
            "--plan-out", str(output), "--info-out", str(info),
        ], env=environment, capture_output=True, text=True, timeout=10, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(output.read_text()), json.loads((context / "railpack-plan.json").read_text()))
        self.assertNotIn(self.proxy, output.read_text())


if __name__ == "__main__":
    unittest.main()

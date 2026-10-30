"""Exercise the synthetic Git/probe fixture before spending time on a k3d build."""

import contextlib
import http.server
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock


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


class RegistryOpener:
    """A strict registry HTTP stub: an incorrect repository/tag path cannot pass."""

    digest = "sha256:" + "a" * 64
    config_digest = "sha256:" + "b" * 64
    base = "http://registry.test:5000/v2/team/app"

    def __init__(self, image_env):
        self.image_env = image_env
        self.requests = []

    def open(self, request, timeout):
        url = request.full_url if hasattr(request, "full_url") else request
        self.requests.append(url)
        if url == self.base + "/manifests/" + self.digest:
            body = {"schemaVersion": 2, "config": {"digest": self.config_digest}}
        elif url == self.base + "/blobs/" + self.config_digest:
            body = {"config": {"Env": self.image_env}}
        else:
            raise AssertionError("unexpected registry URL: " + url)
        if timeout != 15:
            raise AssertionError("registry requests must remain bounded")
        return io.BytesIO(json.dumps(body).encode())


class RegistryInspectionTests(unittest.TestCase):
    def test_tag_and_digest_references_use_the_same_repository_api_paths(self):
        for tag in ("", ":build-123"):
            with self.subTest(tag=tag):
                reference = "registry.test:5000/team/app" + tag + "@" + RegistryOpener.digest
                opener = RegistryOpener(["PATH=/usr/local/bin:/usr/bin:/bin", "HOME=/root"])
                output = io.StringIO()
                with mock.patch.object(MOCK, "direct_opener", return_value=opener), contextlib.redirect_stdout(output):
                    MOCK.inspect_image(reference)
                self.assertEqual(opener.requests, [
                    RegistryOpener.base + "/manifests/" + RegistryOpener.digest,
                    RegistryOpener.base + "/blobs/" + RegistryOpener.config_digest,
                ])
                self.assertIn("IMAGE_ENV_OK " + reference, output.getvalue())

    def test_each_proxy_variable_in_final_image_environment_is_rejected(self):
        for name in sorted(MOCK.PROXY_NAMES):
            with self.subTest(name=name):
                opener = RegistryOpener([name + "=must-not-persist", "PATH=/bin"])
                with mock.patch.object(MOCK, "direct_opener", return_value=opener), self.assertRaises(SystemExit) as error:
                    MOCK.inspect_image("registry.test:5000/team/app:tag@" + RegistryOpener.digest)
                self.assertIn("persisted", str(error.exception))
                self.assertNotIn("must-not-persist", str(error.exception))

    def test_empty_final_image_environment_is_allowed(self):
        opener = RegistryOpener(None)
        with mock.patch.object(MOCK, "direct_opener", return_value=opener), contextlib.redirect_stdout(io.StringIO()):
            MOCK.inspect_image("registry.test:5000/team/app@" + RegistryOpener.digest)


class BackendStartupTests(unittest.TestCase):
    def test_backend_must_exist_and_be_executable(self):
        with tempfile.TemporaryDirectory() as directory:
            backend = Path(directory) / "git-http-backend"
            with mock.patch.object(MOCK.subprocess, "check_output", return_value=directory + "\n"):
                with self.assertRaisesRegex(SystemExit, "executable git-http-backend"):
                    MOCK.git_http_backend()
                backend.write_text("#!/bin/sh\nexit 0\n")
                backend.chmod(0o600)
                with self.assertRaisesRegex(SystemExit, "executable git-http-backend"):
                    MOCK.git_http_backend()
                backend.chmod(0o700)
                self.assertEqual(MOCK.git_http_backend(), str(backend))

    def test_missing_backend_stops_before_readiness_listener_is_created(self):
        with mock.patch.object(MOCK.sys, "argv", ["mockproxy.py"]), \
                mock.patch.object(MOCK, "git_http_backend", side_effect=SystemExit("missing backend")), \
                mock.patch.object(MOCK, "repositories") as repositories, \
                mock.patch.object(MOCK.http.server, "ThreadingHTTPServer") as server:
            with self.assertRaisesRegex(SystemExit, "missing backend"):
                MOCK.main()
            repositories.assert_not_called()
            server.assert_not_called()


if __name__ == "__main__":
    unittest.main()

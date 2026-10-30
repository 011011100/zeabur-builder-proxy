"""Watcher publication and CRI verification regressions, without Docker or Kubernetes."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("check_proxy_import", ROOT / "scripts/check-proxy-import.py")
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


def completed(stdout="", code=0):
    return subprocess.CompletedProcess([], code, stdout, "")


def fixture(root, archives=("images/shared.tar", "images/planner.tar")):
    for relative in archives:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(relative.encode())
    return {"images": [
        {"role": role, "tag": f"ghcr.io/example/{role}:test",
         "digest_reference": f"ghcr.io/example/{role}@sha256:{index:064x}",
         "archive": archives[min(index, len(archives) - 1)]}
        for index, role in enumerate(("controller", "job", "planner"))
    ]}


class ImportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name) / "bundle"
        self.root.mkdir()
        self.manifest = fixture(self.root)
        self.commands = []

    def tearDown(self):
        self.temp.cleanup()

    def runner(self, args, **_kwargs):
        self.commands.append(args)
        return completed()

    def verifier(self, **kwargs):
        return CHECK.Verifier(self.root, self.manifest, "node", runner=self.runner, **kwargs)

    def test_all_roles_and_both_references_are_grouped(self):
        plan = CHECK.archive_plan(self.root, self.manifest)
        self.assertEqual(len(plan), 2)
        self.assertEqual({ref for group in plan for ref in group["references"]},
                         {ref for image in self.manifest["images"]
                          for ref in (image["tag"], image["digest_reference"])})
        self.assertEqual(len({entry["destination"] for entry in plan}), 2)

    def test_only_complete_archives_are_published_inside_node(self):
        verifier = self.verifier()
        verifier.stage()
        copies = [args for args in self.commands if args[:2] == ["docker", "cp"]]
        moves = [args for args in self.commands if "mv" in args]
        self.assertEqual(len(copies), 2)
        self.assertEqual(len(moves), 2)
        self.assertTrue(all(args[-1].endswith(".partial") for args in copies))
        self.assertEqual(len({args[-1] for args in copies}), 2)
        for copy, move in zip(copies, moves):
            self.assertLess(self.commands.index(copy), self.commands.index(move))
            self.assertEqual(copy[-1].split(":", 1)[1], move[-2])
            self.assertTrue(move[-1].endswith(".oci.tar"))
        self.assertFalse(any("import" in args or "label" in args for args in self.commands))

    def test_archive_cannot_escape_bundle(self):
        self.manifest["images"][0]["archive"] = "../external.tar"
        with self.assertRaises(ValueError):
            CHECK.archive_plan(self.root, self.manifest)

    def test_cri_requires_a_real_id_and_boolean_pinned(self):
        self.assertEqual(CHECK.cri_status(completed(code=1))["state"], "missing")
        self.assertEqual(CHECK.cri_status(completed("bad json"))["state"], "invalid-cri-response")
        for status in ({}, {"pinned": True}, {"id": "", "pinned": True}):
            self.assertEqual(CHECK.cri_status(completed(json.dumps({"status": status})))["state"],
                             "missing-image-id")
        for value in (False, None, "true"):
            status = {"id": "sha256:configuration", "pinned": value}
            self.assertEqual(CHECK.cri_status(completed(json.dumps({"status": status})))["state"],
                             "unpinned")
        result = CHECK.cri_status(completed(json.dumps({"status": {"id": "x", "pinned": True},
                                                       "info": {"env": ["PRIVATE=do-not-save"]}})))
        self.assertEqual(result["state"], "pinned")
        self.assertNotIn("PRIVATE", json.dumps(result))

    def test_metadata_tracks_exact_own_aliases_and_labels(self):
        own = "ghcr.io/example/builder:test"
        output = ("REF TYPE DIGEST SIZE PLATFORMS LABELS\n"
                  f"{own} oci sha256:abc 12.3 MiB linux/amd64 io.cattle.k3s.pinned=pinned,io.cri-containerd.pinned=pinned\n"
                  "unrelated:test oci sha256:other 1 KiB linux/amd64 private=not-copied\n")
        metadata = CHECK.image_metadata(output, {own})
        self.assertEqual(set(metadata), {own})
        record = {"state": "pinned", "id": "x", "pinned": True, "containerd": metadata[own]}
        self.assertTrue(CHECK.Verifier.ready(record))
        record["containerd"] = None
        self.assertFalse(CHECK.Verifier.ready(record))

    def test_unpinned_existing_image_is_not_reported_as_missing_or_accepted(self):
        verifier = self.verifier(timeout=0)
        record = {"state": "unpinned", "id": "x", "pinned": False,
                  "containerd": {"labels": {CHECK.PIN: "pinned", CHECK.OWNER: "pinned"}}}
        with mock.patch.object(verifier, "snapshot", return_value={"own:tag": record}), \
                mock.patch.object(verifier, "evidence") as evidence:
            with self.assertRaisesRegex(RuntimeError, "own:tag: unpinned"):
                verifier.wait_pinned("first")
            evidence.assert_called_once_with("first", {"own:tag": record})

    def test_waits_for_real_watcher_metadata_and_cri_updates(self):
        verifier = self.verifier(timeout=2)
        ready = {"state": "pinned", "id": "x", "pinned": True,
                 "containerd": {"labels": {CHECK.PIN: "pinned", CHECK.OWNER: "pinned"}}}
        snapshots = [{"own:tag": {"state": "missing"}},
                     {"own:tag": dict(ready, containerd=None)}, {"own:tag": ready}]
        with mock.patch.object(verifier, "snapshot", side_effect=snapshots) as snapshot, \
                mock.patch.object(verifier, "evidence") as evidence, mock.patch.object(CHECK.time, "sleep"):
            verifier.wait_pinned("recovered")
        self.assertEqual(snapshot.call_count, 3)
        evidence.assert_called_once_with("recovered", {"own:tag": ready})

    def test_recovery_removes_only_manifest_aliases(self):
        verifier = self.verifier()
        verifier.remove_own_aliases()
        removed = [args[-1] for args in self.commands if "rm" in args]
        self.assertEqual(set(removed), verifier.references)
        self.assertEqual(len(removed), 6)

    def test_refresh_always_advances_even_future_mtime(self):
        verifier = self.verifier()
        previous = 2_100_000_000
        def execute(*args, **_kwargs):
            self.assertEqual(args[:3], ("stat", "-c", "%Y"))
            return completed(str(previous))
        with mock.patch.object(verifier, "execute", side_effect=execute), \
                mock.patch.object(CHECK.time, "time", return_value=previous - 100):
            verifier.refresh_retained_archives()
        expected = CHECK.datetime.fromtimestamp(previous + 2, CHECK.timezone.utc).strftime("%Y%m%d%H%M.%S")
        touches = [args for args in self.commands if "touch" in args]
        self.assertEqual(len(touches), 2)
        for args in touches:
            self.assertEqual(args[:5], ["docker", "exec", "-e", "TZ=UTC", "node"])
            self.assertEqual(args[-2], expected)
            self.assertIn(args[-1], {entry["destination"] for entry in verifier.archives})


if __name__ == "__main__":
    unittest.main()

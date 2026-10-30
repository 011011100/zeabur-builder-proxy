"""Failure-boundary tests for the offline Builder installer, with no live cluster."""

import base64
import copy
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("builder_proxy_install", Path(__file__).with_name("install.py"))
install = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(install)


def chart_bytes(extra_name=None):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w:gz") as archive:
        files = {"zeabur-builder/Chart.yaml": b"name: zeabur-builder\nversion: 0.1.1-mingxing.1\n",
                 "zeabur-builder/values.yaml": b"image: {}\n",
                 "zeabur-builder/templates/keep.yaml": b"unchanged\n"}
        if extra_name:
            files[extra_name] = b"unsafe\n"
        for name, data in files.items():
            member = tarfile.TarInfo(name)
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
    return stream.getvalue()


def defaults():
    return {"image": {"repository": "ghcr.io/zeabur-oss/builder", "digest": "", "tag": "", "pullPolicy": "IfNotPresent"},
            "jobImage": {"repository": "ghcr.io/zeabur-oss/builder-job", "digest": "", "tag": ""},
            "proxy": {"url": "", "noProxy": install.NO_PROXY},
            "registry": {"host": "", "ca": ""}}


def make_bundle(root):
    (root / "products").mkdir()
    (root / "images").mkdir()
    chart = chart_bytes()
    (root / "products/chart.tgz").write_bytes(chart)
    (root / "products/defaults.json").write_text(json.dumps(defaults()))
    (root / "images/components.oci.tar").write_bytes(b"test-archive")
    manifest = {"version": "0.1.1-mingxing.1", "source_commit": "a" * 40,
                "chart": "products/chart.tgz", "chart_sha256": install.sha256(chart),
                "defaults": "products/defaults.json", "images": []}
    for role, digit in (("controller", "b"), ("job", "c")):
        manifest["images"].append({"role": role, "tag": "ghcr.io/example/{}:version".format(role),
                                   "digest_reference": "ghcr.io/example/{}@sha256:{}".format(role, digit * 64),
                                   "archive": "images/components.oci.tar"})
    (root / "bundle.json").write_text(json.dumps(manifest))
    checksum_bundle(root)
    return manifest


def checksum_bundle(root):
    lines = []
    for path in sorted(root.rglob("*")):
        if path.is_file() and path.name != "SHA256SUMS":
            lines.append("{}  {}".format(install.file_hash(path), path.relative_to(root).as_posix()))
    (root / "SHA256SUMS").write_text("\n".join(lines) + "\n")


class FakeKube:
    def __init__(self):
        self.node_ip = install.EXPECTED_NODE_IP
        self.namespaces = ["default", "builder-system"]
        self.legacy_dns = False
        self.chart = {"apiVersion": "helm.cattle.io/v1", "kind": "HelmChart",
                      "metadata": {"name": install.CHART_NAME, "namespace": install.CHART_NAMESPACE,
                                   "resourceVersion": "12", "annotations": {"keep": "yes"}},
                      "spec": {"chartContent": base64.b64encode(b"official").decode(),
                               "valuesContent": '{"registry":{"host":"zot.zot.svc:5000"}}',
                               "set": {"unchanged": "true"}}}
        self.config = None
        self.image = install.ORIGINAL_IMAGE

    def get(self, resource, name=None, namespace=None, missing=False, selector=None):
        if resource == "nodes":
            return {"items": [{"metadata": {"name": "zeabur"},
                               "status": {"addresses": [{"type": "InternalIP", "address": self.node_ip}]}}]}
        if resource == "namespaces":
            return {"items": [{"metadata": {"name": item}} for item in self.namespaces]}
        if resource == "service":
            return {"metadata": {"name": "zeabur-dns"}} if self.legacy_dns else None
        if resource == "helmchart":
            return self.chart
        if resource == "helmchartconfig":
            return self.config
        if resource == "deployment":
            return {"spec": {"template": {"spec": {"containers": [{"name": "controller", "image": self.image}]}}}}
        if resource == "statefulsets":
            return {"items": []}
        raise AssertionError("Unexpected read: " + resource)

    def yaml_values(self, text):
        return json.loads(text) if text.strip() else {}


class ProxyValidationTests(unittest.TestCase):
    def test_proxy_endpoint_is_canonical_and_has_no_credentials(self):
        self.assertEqual(install.proxy_endpoint("http://100.66.31.168:3128/"), "http://100.66.31.168:3128")
        for value in ("https://100.66.31.168:3128", "http://user:secret@100.66.31.168:3128",
                      "http://100.66.31.168:65536", "http://100.66.31.168:0",
                      "http://256.66.31.168:3128", "http://100.066.31.168:3128",
                      "http://100.66.31.168:03128", "http://100.66.31.168:3128/path",
                      "http://100.66.31.168:3128?query=secret", "http://127.0.0.1:3128"):
            with self.subTest(value=value), self.assertRaises(install.InstallError) as error:
                install.proxy_endpoint(value)
            self.assertNotIn("secret", str(error.exception))

    def test_bypass_list_retains_internal_routes(self):
        self.assertIn("10.43.0.1", install.bypass_list(install.NO_PROXY))
        self.assertEqual(install.bypass_list(install.NO_PROXY + ",localhost"), install.NO_PROXY)
        for value in ("", "*", "github.com", install.NO_PROXY + ",", install.NO_PROXY + ", example.com"):
            with self.subTest(value=value), self.assertRaises(install.InstallError):
                install.bypass_list(value)


class BundleTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.manifest = make_bundle(self.root)

    def test_valid_bundle_and_role_driven_image_defaults(self):
        self.assertEqual(install.verify_bundle(self.root), self.manifest)
        wanted = install.desired_defaults(self.root, self.manifest, "http://100.66.31.168:3128", install.NO_PROXY)
        self.assertEqual(wanted["image"]["repository"], "ghcr.io/example/controller")
        self.assertEqual(wanted["jobImage"]["digest"], "sha256:" + "c" * 64)
        self.assertEqual(wanted["proxy"]["url"], "http://100.66.31.168:3128")

    def test_changed_file_and_unlisted_file_are_rejected(self):
        (self.root / "images/components.oci.tar").write_bytes(b"corrupt")
        with self.assertRaisesRegex(install.InstallError, "SHA256"):
            install.verify_bundle(self.root)
        checksum_bundle(self.root)
        (self.root / "unlisted").write_text("unexpected")
        with self.assertRaisesRegex(install.InstallError, "absent"):
            install.verify_bundle(self.root)

    def test_archive_paths_and_checksum_traversal_are_rejected(self):
        with (self.root / "SHA256SUMS").open("a") as stream:
            stream.write("{}  ../outside\n".format("0" * 64))
        with self.assertRaisesRegex(install.InstallError, "unsafe"):
            install.verify_bundle(self.root)
        (self.root / "products/chart.tgz").write_bytes(chart_bytes("../escape"))
        with self.assertRaisesRegex(install.InstallError, "unsafe"):
            install.rewritten_chart(self.root / "products/chart.tgz", defaults())

    def test_symlink_and_unpinned_image_are_rejected(self):
        (self.root / "link").symlink_to(self.root / "bundle.json")
        with self.assertRaisesRegex(install.InstallError, "symbolic"):
            install.verify_bundle(self.root)
        (self.root / "link").unlink()
        self.manifest["images"][0]["digest_reference"] = "ghcr.io/example/controller:latest"
        (self.root / "bundle.json").write_text(json.dumps(self.manifest))
        checksum_bundle(self.root)
        with self.assertRaisesRegex(install.InstallError, "pinned"):
            install.verify_bundle(self.root)

    def test_chart_rewrite_changes_only_defaults(self):
        wanted = defaults()
        wanted["proxy"]["url"] = "http://100.66.31.168:3128"
        result = install.rewritten_chart(self.root / "products/chart.tgz", wanted)
        with tarfile.open(fileobj=io.BytesIO(result), mode="r:gz") as archive:
            self.assertEqual(json.load(archive.extractfile("zeabur-builder/values.yaml")), wanted)
            self.assertEqual(archive.extractfile("zeabur-builder/templates/keep.yaml").read(), b"unchanged\n")


class ClusterAndOverrideTests(unittest.TestCase):
    def setUp(self):
        self.kube = FakeKube()
        self.manifest = {"source_commit": "a" * 40, "version": "0.1.1-mingxing.1",
                         "images": [{"role": "controller", "digest_reference": "ghcr.io/example/controller@sha256:" + "b" * 64}]}

    def guard(self):
        with mock.patch.object(install, "ORIGINAL_CHART_SHA256", install.sha256(b"official")):
            return install.guarded_cluster(self.kube, self.manifest)

    def test_new_cluster_guard_and_preservation(self):
        state = self.guard()
        changed = install.desired_chart(state, self.manifest, b"maintained")
        self.assertEqual(changed["spec"]["valuesContent"], self.kube.chart["spec"]["valuesContent"])
        self.assertEqual(changed["spec"]["set"], {"unchanged": "true"})
        self.assertEqual(changed["spec"]["version"], self.manifest["version"])
        self.assertEqual(changed["metadata"]["annotations"]["keep"], "yes")
        self.assertNotIn("resourceVersion", changed["metadata"])
        self.assertEqual(self.kube.chart["metadata"]["resourceVersion"], "12")

    def test_legacy_cluster_and_unknown_images_are_refused(self):
        for alteration in (lambda: setattr(self.kube, "node_ip", "172.16.1.124"),
                           lambda: self.kube.namespaces.append("environment-old"),
                           lambda: setattr(self.kube, "legacy_dns", True),
                           lambda: setattr(self.kube, "image", "ghcr.io/other/controller:unknown")):
            self.kube = FakeKube()
            alteration()
            with self.assertRaises(install.InstallError):
                self.guard()

    def test_conflicting_values_and_set_overrides_are_refused(self):
        wanted = defaults()
        state = {"helmchart": copy.deepcopy(self.kube.chart), "helmchartconfig": None}
        install.check_overrides(self.kube, state, wanted)
        for content in ('{"image":{"repository":"other"}}', '{"proxy":{"url":"http://other:3128"}}',
                        '{"jobImage":null}'):
            state["helmchart"]["spec"]["valuesContent"] = content
            with self.subTest(content=content), self.assertRaises(install.InstallError):
                install.check_overrides(self.kube, state, wanted)
        state["helmchart"]["spec"]["valuesContent"] = '{"image":{"pullPolicy":"IfNotPresent"}}'
        state["helmchart"]["spec"]["set"] = {"image.repository": "other"}
        with self.assertRaises(install.InstallError):
            install.check_overrides(self.kube, state, wanted)
        state["helmchart"]["spec"]["set"] = {"image.repository": wanted["image"]["repository"]}
        install.check_overrides(self.kube, state, wanted)

    def test_secret_backed_values_are_not_read_or_silently_ignored(self):
        state = {"helmchart": copy.deepcopy(self.kube.chart), "helmchartconfig": None}
        state["helmchart"]["spec"]["valuesSecrets"] = [{"name": "do-not-read"}]
        with self.assertRaisesRegex(install.InstallError, "valuesSecrets"):
            install.check_overrides(self.kube, state, defaults())

    def test_yaml_parser_fails_closed_if_unknown_fields_are_discarded(self):
        kube = install.Kube()
        with mock.patch.object(kube, "command", return_value=mock.Mock(stdout='{"x-values":{"image":{"tag":"x"}}}')) as command:
            self.assertEqual(kube.yaml_values("image:\n  tag: x\n"), {"image": {"tag": "x"}})
            args = command.call_args.args
            self.assertIn("--dry-run=client", args)
            self.assertIn("--validate=false", args)
        with mock.patch.object(kube, "command", return_value=mock.Mock(stdout='{}')):
            with self.assertRaisesRegex(install.InstallError, "mapping"):
                kube.yaml_values("image:\n  tag: x\n")

    def test_dry_run_cannot_be_mistaken_for_apply(self):
        kube = install.Kube()
        with mock.patch.object(kube, "command") as command:
            kube.apply(self.kube.chart, dry_run=True)
            self.assertIn("--dry-run=server", command.call_args.args)
            self.assertEqual(json.loads(command.call_args.kwargs["data"]), self.kube.chart)

    def test_apply_scope_cannot_include_business_workloads_or_secrets(self):
        kube = install.Kube()
        for kind, name, namespace in (("Deployment", "byts-tos", "mingxing"),
                                      ("Secret", "github-app", "builder-system"),
                                      ("HelmChart", "other", "kube-system"),
                                      ("StatefulSet", "database", "mingxing")):
            with self.subTest(kind=kind), mock.patch.object(kube, "command") as command:
                with self.assertRaisesRegex(install.InstallError, "outside"):
                    kube.apply({"kind": kind, "metadata": {"name": name, "namespace": namespace}})
                command.assert_not_called()


if __name__ == "__main__":
    unittest.main()

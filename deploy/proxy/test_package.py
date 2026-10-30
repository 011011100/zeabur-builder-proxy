"""Exercise the offline package boundary with small synthetic Linux OCI layouts."""

import hashlib
import importlib.util
import io
import json
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
PACKAGE_SPEC = importlib.util.spec_from_file_location("proxy_package", ROOT / "scripts/package-proxy.py")
package = importlib.util.module_from_spec(PACKAGE_SPEC)
PACKAGE_SPEC.loader.exec_module(package)
INSTALL_SPEC = importlib.util.spec_from_file_location("proxy_package_install", Path(__file__).with_name("install.py"))
install = importlib.util.module_from_spec(INSTALL_SPEC)
INSTALL_SPEC.loader.exec_module(install)


def blob(directory, body, media_type):
    digest = hashlib.sha256(body).hexdigest()
    path = directory / "blobs/sha256" / digest
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body)
    return {"mediaType": media_type, "digest": "sha256:" + digest, "size": len(body)}


def image_layout(directory, role):
    config = {"architecture": "amd64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": []},
              "config": {"Labels": {"fixture-role": role}}}
    config_descriptor = blob(directory, json.dumps(config).encode(), "application/vnd.oci.image.config.v1+json")
    manifest = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": config_descriptor, "layers": []}
    descriptor = blob(directory, json.dumps(manifest).encode(), "application/vnd.oci.image.manifest.v1+json")
    (directory / "oci-layout").write_text('{"imageLayoutVersion":"1.0.0"}')
    (directory / "index.json").write_text(json.dumps({"schemaVersion": 2, "manifests": [descriptor]}))


class ThreeImagePackageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.images = self.root / "images"
        self.output = self.root / "output"
        self.version = "0.1.1-mingxing.1"
        for name in ("builder", "builder-job", "builder-railpack"):
            image_layout(self.images / name, name)
        self.defaults = {name: {"repository": "ghcr.io/zeabur-oss/" + repository,
                                "tag": "", "digest": ""}
                         for name, repository in (("image", "builder"), ("jobImage", "builder-job"),
                                                  ("railpackImage", "builder-railpack"))}
        self.defaults["proxy"] = {"url": "", "noProxy": install.NO_PROXY}
        self.defaults_file = self.root / "defaults.json"
        self.defaults_file.write_text(json.dumps(self.defaults))
        self.chart_file = self.root / "chart.tgz"
        with tarfile.open(self.chart_file, "w:gz") as chart:
            content = json.dumps(self.defaults).encode()
            member = tarfile.TarInfo("zeabur-builder/values.yaml")
            member.size = len(content)
            chart.addfile(member, io.BytesIO(content))

    def arguments(self):
        return ["--images", str(self.images), "--controller-ref", "ghcr.io/example/builder:maintained",
                "--job-ref", "ghcr.io/example/builder-job:maintained", "--planner-ref",
                "ghcr.io/example/builder-railpack:maintained", "--chart", str(self.chart_file),
                "--defaults", str(self.defaults_file), "--version", self.version,
                "--commit", "a" * 40, "--output", str(self.output)]

    def bundle(self):
        return self.output / ("zeabur-builder-proxy-" + self.version + "-linux-amd64")

    def test_all_three_images_enter_checksum_manifest_and_oci_archive(self):
        with mock.patch("builtins.print"):
            package.main(self.arguments())
        manifest = install.verify_bundle(self.bundle())
        self.assertEqual({image["role"] for image in manifest["images"]}, {"controller", "job", "planner"})
        self.assertEqual(len(manifest["images"]), 3)
        tags = {image["tag"] for image in manifest["images"]}
        with tarfile.open(self.bundle() / "images/components-amd64.oci.tar") as archive:
            index = json.load(archive.extractfile("index.json"))
            self.assertEqual({item["annotations"]["org.opencontainers.image.ref.name"]
                              for item in index["manifests"]}, tags)
            self.assertEqual(len(index["manifests"]), 3)
            descriptors = {item["annotations"]["org.opencontainers.image.ref.name"]: item
                           for item in index["manifests"]}
            for image in manifest["images"]:
                self.assertEqual(image["digest_reference"], image["tag"].rsplit(":", 1)[0]
                                 + "@" + descriptors[image["tag"]]["digest"])
        defaults = install.desired_defaults(self.bundle(), manifest, "http://100.66.31.168:3128", install.NO_PROXY)
        for role, key in install.IMAGE_SETTINGS.items():
            record = next(image for image in manifest["images"] if image["role"] == role)
            self.assertEqual(defaults[key]["repository"] + "@" + defaults[key]["digest"],
                             record["digest_reference"])
        self.assertEqual(defaults["railpackImage"]["repository"], "ghcr.io/example/builder-railpack")
        self.assertEqual(json.loads((self.bundle() / "products/default-values.json").read_text()), self.defaults)
        self.assertTrue((self.output / (self.bundle().name + ".tar.gz.sha256")).is_file())

    def test_missing_planner_layout_cannot_publish_an_installable_bundle(self):
        shutil.rmtree(self.images / "builder-railpack")
        with mock.patch("builtins.print"), self.assertRaises((OSError, ValueError)):
            package.main(self.arguments())
        self.assertFalse((self.output / (self.bundle().name + ".tar.gz")).exists())


if __name__ == "__main__":
    unittest.main()

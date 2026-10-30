"""Content-closure checks for the installation bundle, using tiny OCI fixtures."""

import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("package_proxy", ROOT / "scripts/package-proxy.py")
PACKAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGE)

OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
REF = "ghcr.io/example/zeabur-builder-proxy/builder:test"


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def blob(source, content, media_type):
    digest = hashlib.sha256(content).hexdigest()
    path = source / "blobs/sha256" / digest
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)
    return {"mediaType": media_type, "digest": "sha256:" + digest, "size": len(content)}


def save_manifest(source, manifest):
    descriptor = blob(source, encoded(manifest), OCI_MANIFEST)
    descriptor["platform"] = {"architecture": "amd64", "os": "linux"}
    (source / "index.json").write_bytes(encoded({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [descriptor],
    }))
    return descriptor


def image(source, arch="amd64"):
    source.mkdir(parents=True)
    (source / "oci-layout").write_bytes(encoded({"imageLayoutVersion": "1.0.0"}))
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w"):
        pass
    layer = blob(source, gzip.compress(raw.getvalue(), mtime=0),
                 "application/vnd.oci.image.layer.v1.tar+gzip")
    config = blob(source, encoded({
        "architecture": arch,
        "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": ["sha256:" + hashlib.sha256(raw.getvalue()).hexdigest()]},
    }), "application/vnd.oci.image.config.v1+json")
    manifest = {"schemaVersion": 2, "mediaType": OCI_MANIFEST,
                "config": config, "layers": [layer]}
    descriptor = save_manifest(source, manifest)
    return manifest, descriptor


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        self.manifest, self.descriptor = image(self.source)
        self.merged = self.root / "merged"

    def tearDown(self):
        self.temporary.cleanup()

    def merge(self):
        return PACKAGE.merge_image(self.source, self.merged, REF)

    def test_tags_and_digest_refs_name_the_same_checked_manifest(self):
        entries, record = self.merge()
        expected_digest = self.descriptor["digest"]
        self.assertEqual(record["tag"], REF)
        self.assertEqual(record["digest_reference"], REF.rsplit(":", 1)[0] + "@" + expected_digest)
        self.assertEqual([entry["digest"] for entry in entries], [expected_digest, expected_digest])
        self.assertEqual([entry["annotations"]["org.opencontainers.image.ref.name"] for entry in entries],
                         [record["tag"], record["digest_reference"]])
        for path in (self.merged / "blobs/sha256").iterdir():
            self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), path.name)

    def test_corrupt_blob_is_rejected(self):
        digest = self.manifest["layers"][0]["digest"].split(":")[1]
        (self.source / "blobs/sha256" / digest).write_bytes(b"not the checksummed layer")
        with self.assertRaises(ValueError):
            self.merge()

    def test_missing_referenced_layer_is_rejected(self):
        digest = self.manifest["layers"][0]["digest"].split(":")[1]
        (self.source / "blobs/sha256" / digest).unlink()
        with self.assertRaises(ValueError):
            self.merge()

    def test_config_descriptor_size_is_checked(self):
        self.manifest["config"]["size"] += 1
        save_manifest(self.source, self.manifest)
        with self.assertRaises(ValueError):
            self.merge()

    def test_layer_descriptor_size_is_checked(self):
        self.manifest["layers"][0]["size"] += 1
        save_manifest(self.source, self.manifest)
        with self.assertRaises(ValueError):
            self.merge()

    def test_manifest_descriptor_size_is_checked(self):
        index = json.loads((self.source / "index.json").read_text())
        index["manifests"][0]["size"] += 1
        (self.source / "index.json").write_bytes(encoded(index))
        with self.assertRaises(ValueError):
            self.merge()

    def test_config_descriptor_cannot_traverse_outside_oci_content(self):
        self.manifest["config"]["digest"] = "sha256:../../external-config"
        save_manifest(self.source, self.manifest)
        with self.assertRaises(ValueError):
            self.merge()

    def test_non_sha256_layer_is_rejected(self):
        self.manifest["layers"][0]["digest"] = "sha512:" + "a" * 128
        save_manifest(self.source, self.manifest)
        with self.assertRaises(ValueError):
            self.merge()

    def test_architecture_is_read_from_verified_config(self):
        other = self.root / "arm64"
        image(other, arch="arm64")
        with self.assertRaises(ValueError):
            PACKAGE.merge_image(other, self.merged, REF)

    def test_release_tar_has_the_complete_reference_and_checksum_closure(self):
        images = self.root / "images"
        for name in ("builder", "builder-job"):
            image(images / name)
        chart = self.root / "chart.tgz"
        with tarfile.open(chart, "w:gz") as archive:
            data = b"apiVersion: v2\nname: zeabur-builder\nversion: 0.1.1-mingxing.1\n"
            member = tarfile.TarInfo("zeabur-builder/Chart.yaml")
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))
        defaults = self.root / "defaults.json"
        defaults.write_text("{}\n")
        output = self.root / "output/artifacts"
        result = subprocess.run([
            sys.executable, str(ROOT / "scripts/package-proxy.py"),
            "--images", str(images), "--controller-ref", REF,
            "--job-ref", REF.replace("/builder:", "/builder-job:"),
            "--chart", str(chart), "--defaults", str(defaults),
            "--version", "0.1.1-mingxing.1", "--commit", "a" * 40,
            "--output", str(output),
        ], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        target, = output.glob("*.tar.gz")
        expected_sha = target.with_name(target.name + ".sha256").read_text().split()[0]
        self.assertEqual(expected_sha, hashlib.sha256(target.read_bytes()).hexdigest())
        with tarfile.open(target, "r:gz") as archive:
            contents = {member.name: archive.extractfile(member).read()
                        for member in archive.getmembers() if member.isfile()}
        root_name = "zeabur-builder-proxy-0.1.1-mingxing.1-linux-amd64/"
        contents = {name.removeprefix(root_name): content for name, content in contents.items()}
        checksums = contents.pop("SHA256SUMS").decode().splitlines()
        self.assertEqual({line.split("  ", 1)[1] for line in checksums}, set(contents))
        for line in checksums:
            digest, name = line.split("  ", 1)
            self.assertEqual(digest, hashlib.sha256(contents[name]).hexdigest())
        manifest = json.loads(contents["bundle.json"])
        self.assertEqual(manifest["source_commit"], "a" * 40)
        self.assertEqual({record["role"] for record in manifest["images"]}, {"controller", "job"})
        with tarfile.open(fileobj=io.BytesIO(contents["images/components-amd64.oci.tar"]), mode="r:") as archive:
            index = json.load(archive.extractfile("index.json"))
            refs = {entry["annotations"]["org.opencontainers.image.ref.name"]: entry["digest"]
                    for entry in index["manifests"]}
            for record in manifest["images"]:
                self.assertIn(record["tag"], refs)
                self.assertIn(record["digest_reference"], refs)
                self.assertEqual(refs[record["tag"]], refs[record["digest_reference"]])
                for ref in (record["tag"], record["digest_reference"]):
                    self.assertIn("blobs/" + refs[ref].replace(":", "/"), archive.getnames())


if __name__ == "__main__":
    unittest.main()

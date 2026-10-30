#!/usr/bin/env python3
"""Verify every packaged OCI tag and digest through a disposable K3s node's CRI."""
import json
from pathlib import Path
import subprocess
import sys

root = Path(sys.argv[1]).resolve()
manifest = json.loads((root / "bundle.json").read_text())
node = "k3d-zeabur-builder-server-0"
for archive in sorted({image["archive"] for image in manifest["images"]}):
    source = root / archive
    subprocess.run(["docker", "cp", str(source), node + ":/tmp/mingxing-proxy-import.tar"], check=True)
    subprocess.run(["docker", "exec", node, "k3s", "ctr", "-n", "k8s.io", "images", "import",
                    "/tmp/mingxing-proxy-import.tar"], check=True)
for image in manifest["images"]:
    for reference in (image["tag"], image["digest_reference"]):
        result = subprocess.run(["docker", "exec", node, "k3s", "crictl", "inspecti", reference],
                                text=True, capture_output=True, check=True)
        status = json.loads(result.stdout)["status"]
        if not status.get("id"):
            raise SystemExit("K3s CRI did not identify " + reference)
print("OCI_CRI_OK: every offline controller/job tag and digest is resolvable.")

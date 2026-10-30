#!/usr/bin/env python3
"""Verify every packaged OCI tag and digest through a disposable K3s node's CRI."""
import json
from pathlib import Path
import subprocess
import sys
import time

root = Path(sys.argv[1]).resolve()
manifest = json.loads((root / "bundle.json").read_text())
node = "k3d-zeabur-builder-server-0"
directory = "/var/lib/rancher/k3s/agent/images"
subprocess.run(["docker", "exec", node, "mkdir", "-p", directory], check=True)
for archive in sorted({image["archive"] for image in manifest["images"]}):
    source = root / archive
    subprocess.run(["docker", "cp", str(source), node + ":" + directory + "/mingxing-proxy.oci.tar"], check=True)
references = {ref for image in manifest["images"] for ref in (image["tag"], image["digest_reference"])}


def wait_pinned():
    pending = set(references)
    deadline = time.monotonic() + 180
    while pending and time.monotonic() < deadline:
        for reference in list(pending):
            result = subprocess.run(["docker", "exec", node, "k3s", "crictl", "inspecti", reference],
                                    text=True, capture_output=True, check=False)
            if result.returncode:
                continue
            status = json.loads(result.stdout)["status"]
            if status.get("id") and status.get("pinned") is True:
                pending.remove(reference)
        if pending:
            time.sleep(3)
    if pending:
        raise SystemExit("K3s did not import and pin every offline reference: " + ", ".join(sorted(pending)))


wait_pinned()
# Only this bundle's aliases are removed; the running CI images have separate local names.
# Touching the retained archive must recover aliases and pins without an internet pull.
for reference in sorted(references):
    subprocess.run(["docker", "exec", node, "k3s", "ctr", "-n", "k8s.io", "images", "rm", reference], check=True)
subprocess.run(["docker", "exec", node, "touch", directory + "/mingxing-proxy.oci.tar"], check=True)
wait_pinned()
print("OCI_CRI_OK: K3s imported/pinned every offline reference and recovered them from the retained archive.")

#!/usr/bin/env python3
"""Exercise the installer's YAML parser against kubectl in the disposable CI cluster."""
import importlib.util
import json
from pathlib import Path

root = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("proxy_installer", root / "deploy/proxy/install.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Kubectl(module.Kube):
    """CI has kubectl on PATH; the real server uses the K3s wrapper."""

    def command(self, *arguments, data=None, optional=False, timeout=30):
        return module.run(["kubectl", *arguments], data=data, optional=optional, timeout=timeout)


kube = Kubectl()
expected = {"image": {"repository": "example.invalid/builder", "digest": "sha256:test"},
            "proxy": {"url": "http://192.0.2.10:3128"}, "enabled": False, "ports": [3128, 443]}
yaml = """image:
  repository: example.invalid/builder
  digest: sha256:test
proxy:
  url: http://192.0.2.10:3128
enabled: false
ports: [3128, 443]
"""
if kube.yaml_values(yaml) != expected or kube.yaml_values(json.dumps(expected)) != expected:
    raise SystemExit("kubectl did not preserve the installer's in-memory YAML mapping")
if kube.get("configmap", "builder-proxy-values-parser", "default", missing=True) is not None:
    raise SystemExit("the YAML parser unexpectedly persisted an object")
print("YAML_ROUNDTRIP_OK: nested values preserved and no object persisted.")

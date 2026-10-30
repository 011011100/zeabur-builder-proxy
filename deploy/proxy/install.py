#!/usr/bin/env python3
"""Stage and install the maintained Builder proxy variant on the new K3s server.

Only an explicit --apply changes the cluster. The chart is embedded in the existing
HelmChart, preserving its existing values and HelmChartConfig. No business workload,
Secret, host proxy setting or K3s configuration is modified.
"""

import argparse
import base64
import copy
import datetime
import hashlib
import io
import ipaddress
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.parse


CHART_NAMESPACE = "kube-system"
CHART_NAME = "zeabur-builder"
BUILDER_NAMESPACE = "builder-system"
EXPECTED_NODE_IP = "172.16.1.144"
ORIGINAL_CHART_SHA256 = "b48738f2059e2feccc54245ff3025c7300df33542011a570635a52012fc49bca"
ORIGINAL_IMAGE = "ghcr.io/zeabur-oss/builder@sha256:f964d8b5ca515bc84466971ab02aff86f4b7d6ecdca59f753e85cffbb92e78ed"
SOURCE_ANNOTATION = "mingxingwenhua.cn/builder-proxy-source"
VERSION_ANNOTATION = "mingxingwenhua.cn/builder-proxy-version"
PROXY_NAMES = {"HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy"}
NO_PROXY = "localhost,127.0.0.1,::1,.svc,.svc.cluster.local,10.43.0.1,10.42.0.0/16,10.43.0.0/16,172.16.0.0/12"
BACKUP_ROOT = Path("/root/zeabur-builder-proxy-backups")


class InstallError(Exception):
    """A validation failed before a dependent operation could run."""


def require(condition, message):
    if not condition:
        raise InstallError(message)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def file_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def bundle_path(root, relative):
    require(isinstance(relative, str), "Bundle paths must be strings.")
    posix = PurePosixPath(relative)
    require(relative and not posix.is_absolute() and ".." not in posix.parts,
            "Bundle contains an unsafe path.")
    path = root.joinpath(*posix.parts)
    require(not any(parent.is_symlink() for parent in (path, *path.parents) if parent != root.parent),
            "Bundle paths must not traverse symbolic links.")
    require(path.resolve().is_relative_to(root.resolve()), "Bundle path escapes its directory.")
    return path


def verify_bundle(root):
    root = root.resolve()
    checksums = root / "SHA256SUMS"
    require(checksums.is_file() and not checksums.is_symlink(), "Missing regular SHA256SUMS file.")
    expected = {}
    for line in checksums.read_text().splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"([0-9a-f]{64})  (.+)", line)
        require(match is not None, "Invalid SHA256SUMS entry.")
        digest, relative = match.groups()
        require(relative not in expected, "Duplicate SHA256SUMS path.")
        path = bundle_path(root, relative)
        require(path.is_file(), "A checksummed bundle file is missing.")
        require(file_hash(path) == digest, "Bundle SHA256 verification failed: " + relative)
        expected[relative] = digest
    require(expected, "SHA256SUMS is empty.")
    for path in root.rglob("*"):
        require(not path.is_symlink(), "Bundle must not contain symbolic links.")
        if path.is_file() and path != checksums:
            require(path.relative_to(root).as_posix() in expected,
                    "Bundle contains a file absent from SHA256SUMS.")
    require("bundle.json" in expected, "bundle.json is not checksummed.")
    manifest = json.loads((root / "bundle.json").read_text())
    require(re.fullmatch(r"[0-9a-f]{40}", manifest.get("source_commit", "")),
            "Bundle source_commit must be a full Git commit.")
    require(isinstance(manifest.get("version"), str) and "mingxing" in manifest["version"],
            "Bundle must identify itself as the maintained Mingxing version.")
    for key in ("chart", "defaults"):
        require(manifest.get(key) in expected, key + " is not checksummed.")
    require(expected[manifest["chart"]] == manifest.get("chart_sha256"),
            "Chart SHA256 does not match bundle.json.")
    images = manifest.get("images", [])
    require(len(images) == 2 and {image.get("role") for image in images} == {"controller", "job"},
            "Bundle images must name exactly one controller and one job role.")
    for image in images:
        require(re.fullmatch(r"[a-z0-9][a-z0-9./_:-]*@sha256:[0-9a-f]{64}",
                             image.get("digest_reference", "")), "Image must be pinned by digest.")
        require(re.fullmatch(r"[a-z0-9][a-z0-9./_:-]+", image.get("tag", "")),
                "Image must include its import tag.")
        require(image.get("archive") in expected, "Image archive is not checksummed.")
    return manifest


def proxy_endpoint(value):
    require(isinstance(value, str), "Proxy must be an unauthenticated http://IPv4:port endpoint.")
    parsed = urllib.parse.urlsplit(value)
    require(parsed.scheme == "http" and not parsed.username and not parsed.password
            and not parsed.query and not parsed.fragment and parsed.path in ("", "/"),
            "Proxy must be an unauthenticated http://IPv4:port endpoint without a path.")
    try:
        address = ipaddress.IPv4Address(parsed.hostname or "")
        port = parsed.port
    except ValueError as error:
        raise InstallError("Proxy must contain a valid IPv4 address and TCP port.") from error
    require(port is not None and 1 <= port <= 65535, "Proxy TCP port must be from 1 to 65535.")
    require(not (address.is_loopback or address.is_link_local or address.is_multicast
                 or address.is_unspecified), "Proxy must not be a loopback, link-local or multicast endpoint.")
    canonical = "http://{}:{}".format(address, port)
    require(value.rstrip("/") == canonical, "Proxy must use a canonical IPv4 address and decimal port.")
    return canonical


def bypass_list(value):
    require(isinstance(value, str) and value and not re.search(r"\s", value),
            "NO_PROXY must be a non-empty comma-separated list without whitespace.")
    entries = value.split(",")
    require(all(entries), "NO_PROXY must not contain empty entries.")
    require(set(NO_PROXY.split(",")).issubset(entries),
            "NO_PROXY must retain all default internal cluster addresses and ranges.")
    return ",".join(dict.fromkeys(entries))


def run(command, *, data=None, timeout=30, optional=False):
    try:
        result = subprocess.run(command, input=data, text=True, capture_output=True,
                                timeout=timeout, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise InstallError("Command could not complete: " + " ".join(command[:4])) from error
    if result.returncode and not optional:
        # Do not print arbitrary Kubernetes values, GitHub credentials or chart contents.
        raise InstallError("Command failed: {} (exit {}).".format(" ".join(command[:6]), result.returncode))
    return result


class Kube:
    """The installed K3s CLI is also the YAML parser; no Python packages are required."""

    def command(self, *arguments, data=None, optional=False, timeout=30):
        return run(["k3s", "kubectl", *arguments], data=data, optional=optional, timeout=timeout)

    def get(self, resource, name=None, namespace=None, missing=False, selector=None):
        arguments = ["get", resource]
        if name:
            arguments.append(name)
        if namespace:
            arguments.extend(["-n", namespace])
        if selector:
            arguments.extend(["-l", selector])
        if missing:
            arguments.append("--ignore-not-found")
        arguments.extend(["-o", "json"])
        output = self.command(*arguments).stdout.strip()
        return json.loads(output) if output else None

    def yaml_values(self, text):
        if not text.strip():
            return {}
        try:
            result = json.loads(text)
        except json.JSONDecodeError:
            # Client-side YAML decoding of an in-memory object, with no persistence. Unknown
            # fields are intentionally retained by kubectl's unstructured JSON conversion.
            wrapper = ("apiVersion: v1\nkind: ConfigMap\nmetadata:\n"
                       "  name: builder-proxy-values-parser\nx-values:\n"
                       + "".join("  " + line + "\n" for line in text.splitlines()))
            output = self.command("create", "--dry-run=client", "--validate=false", "-f", "-",
                                  "-o", "json", data=wrapper).stdout
            result = json.loads(output).get("x-values")
        require(isinstance(result, dict), "Helm valuesContent must be a mapping.")
        return result

    def apply(self, document, *, dry_run=False):
        arguments = ["apply", "-f", "-"]
        if dry_run:
            arguments.append("--dry-run=server")
        self.command(*arguments, data=json.dumps(document))


def clean_object(document):
    result = copy.deepcopy(document)
    result.pop("status", None)
    metadata = result.setdefault("metadata", {})
    for name in ("managedFields", "resourceVersion", "uid", "creationTimestamp", "generation"):
        metadata.pop(name, None)
    metadata.get("annotations", {}).pop("kubectl.kubernetes.io/last-applied-configuration", None)
    return result


def guarded_cluster(kube, manifest):
    nodes = kube.get("nodes")["items"]
    require(len(nodes) == 1, "This installer is restricted to the new single-node cluster.")
    addresses = {address.get("address") for address in nodes[0].get("status", {}).get("addresses", [])
                 if address.get("type") == "InternalIP"}
    require(EXPECTED_NODE_IP in addresses, "Node address is not the new server 172.16.1.144; stopped.")
    namespaces = kube.get("namespaces")["items"]
    require(not any(item["metadata"]["name"].startswith("environment-") for item in namespaces),
            "Legacy commercial environment namespaces found; stopped without changes.")
    require(kube.get("service", "zeabur-dns", "default", missing=True) is None,
            "Legacy default/zeabur-dns found; stopped without changes.")
    chart = kube.get("helmchart", CHART_NAME, CHART_NAMESPACE)
    chart_config = kube.get("helmchartconfig", CHART_NAME, CHART_NAMESPACE, missing=True)
    deployment = kube.get("deployment", CHART_NAME, BUILDER_NAMESPACE)
    image = controller_image(deployment)
    annotations = chart.get("metadata", {}).get("annotations", {})
    maintained = annotations.get(SOURCE_ANNOTATION) == manifest["source_commit"]
    expected = next(item["digest_reference"] for item in manifest["images"] if item["role"] == "controller")
    require(image == ORIGINAL_IMAGE or (maintained and image == expected),
            "Current controller is neither the verified official image nor this maintained bundle.")
    content = chart.get("spec", {}).get("chartContent", "")
    try:
        archive = base64.b64decode(content, validate=True)
    except (ValueError, TypeError) as error:
        raise InstallError("Current HelmChart has no valid embedded chartContent.") from error
    require(bool(archive) and (sha256(archive) == ORIGINAL_CHART_SHA256 or maintained),
            "Current chart is not the verified official chart or this bundle's maintained chart.")
    statefulsets = kube.get("statefulsets", namespace=BUILDER_NAMESPACE,
                            selector="app.kubernetes.io/name=buildkitd")["items"]
    return {"helmchart": chart, "helmchartconfig": chart_config,
            "deployment_image": image, "statefulsets": statefulsets,
            "node": nodes[0]["metadata"]["name"], "node_ip": EXPECTED_NODE_IP}


def controller_image(deployment):
    controllers = [container for container in deployment["spec"]["template"]["spec"]["containers"]
                   if container["name"] == "controller"]
    require(len(controllers) == 1, "Cannot identify exactly one Builder controller container.")
    return controllers[0]["image"]


def desired_defaults(root, manifest, proxy, no_proxy):
    defaults = json.loads(bundle_path(root, manifest["defaults"]).read_text())
    for image in manifest["images"]:
        repository, digest = image["digest_reference"].rsplit("@", 1)
        key = "image" if image["role"] == "controller" else "jobImage"
        defaults[key].update(repository=repository, digest=digest, tag="")
    defaults["proxy"] = {"url": proxy, "noProxy": no_proxy}
    return defaults


def matching_subset(actual, desired):
    if isinstance(actual, dict) and isinstance(desired, dict):
        return all(key in desired and matching_subset(value, desired[key]) for key, value in actual.items())
    return type(actual) is type(desired) and actual == desired


def check_overrides(kube, state, defaults):
    for key in ("helmchart", "helmchartconfig"):
        document = state.get(key)
        if not document:
            continue
        spec = document.get("spec", {})
        values = kube.yaml_values(spec.get("valuesContent", ""))
        for section in ("image", "jobImage", "proxy"):
            if section in values:
                require(matching_subset(values[section], defaults[section]),
                        key + " valuesContent overrides " + section + " differently; stopped without replacing it.")
        for path, value in spec.get("set", {}).items():
            parts = path.split(".")
            if parts[0] not in ("image", "jobImage", "proxy"):
                continue
            wanted = defaults
            for part in parts:
                require(isinstance(wanted, dict) and part in wanted,
                        key + " spec.set contains an unsupported proxy/image override.")
                wanted = wanted[part]
            require(str(value) == str(wanted), key + " spec.set conflicts with the maintained image/proxy values.")


def rewritten_chart(path, defaults):
    output = io.BytesIO()
    found = 0
    with tarfile.open(path, "r:gz") as source, tarfile.open(fileobj=output, mode="w:gz") as target:
        for member in source.getmembers():
            parts = PurePosixPath(member.name)
            require(not parts.is_absolute() and ".." not in parts.parts
                    and parts.parts and parts.parts[0] == "zeabur-builder",
                    "Chart contains an unsafe archive path.")
            require(member.isfile() or member.isdir(), "Chart contains an unsupported link or special file.")
            member = copy.copy(member)
            content = source.extractfile(member).read() if member.isfile() else None
            if member.name == "zeabur-builder/values.yaml":
                content = (json.dumps(defaults, indent=2) + "\n").encode()
                member.size = len(content)
                found += 1
            target.addfile(member, io.BytesIO(content) if content is not None else None)
    require(found == 1, "Chart must contain exactly one zeabur-builder/values.yaml.")
    return output.getvalue()


def desired_chart(state, manifest, archive):
    chart = clean_object(state["helmchart"])
    chart["spec"]["chartContent"] = base64.b64encode(archive).decode()
    annotations = chart["metadata"].setdefault("annotations", {})
    annotations[SOURCE_ANNOTATION] = manifest["source_commit"]
    annotations[VERSION_ANNOTATION] = manifest["version"]
    return chart


def private_json(path, document):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as stream:
        json.dump(document, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())


def backup_state(state, manifest):
    BACKUP_ROOT.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(BACKUP_ROOT, 0o700)
    backup = Path(tempfile.mkdtemp(prefix=datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ-"),
                                   dir=BACKUP_ROOT))
    os.chmod(backup, 0o700)
    private_json(backup / "state.json", {"format": 1, "bundle_source": manifest["source_commit"], **state})
    return backup


def wait_images(images, timeout=240):
    pending = {reference for image in images for reference in (image["tag"], image["digest_reference"])}
    deadline = time.monotonic() + timeout
    while pending and time.monotonic() < deadline:
        for reference in list(pending):
            if run(["k3s", "crictl", "inspecti", reference], optional=True, timeout=15).returncode == 0:
                pending.remove(reference)
        if pending:
            print("Waiting for K3s to import {} image reference(s)...".format(len(pending)), flush=True)
            time.sleep(5)
    require(not pending, "K3s did not import all pinned image references before the timeout; chart was not changed.")


def stage_images(root, manifest):
    directory = Path("/var/lib/rancher/k3s/agent/images")
    require(directory.is_dir() and not directory.is_symlink(), "K3s image import directory is unavailable.")
    for index, archive in enumerate(sorted({image["archive"] for image in manifest["images"]})):
        source = bundle_path(root, archive)
        destination = directory / ("mingxing-builder-proxy-{}-{}.oci.tar".format(manifest["source_commit"][:12], index))
        if destination.exists():
            require(destination.is_file() and not destination.is_symlink()
                    and file_hash(destination) == file_hash(source),
                    "An existing staged image archive differs; stopped without overwriting it.")
            continue
        descriptor, temporary = tempfile.mkstemp(prefix=".mingxing-builder-proxy-", dir=directory)
        try:
            with os.fdopen(descriptor, "wb") as target, source.open("rb") as stream:
                shutil.copyfileobj(stream, target, 1024 * 1024)
                target.flush()
                os.fsync(target.fileno())
            os.chmod(temporary, 0o644)
            os.replace(temporary, destination)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
    wait_images(manifest["images"])


def wait_controller(kube, expected, timeout=360):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        deployment = kube.get("deployment", CHART_NAME, BUILDER_NAMESPACE)
        spec, status = deployment["spec"], deployment.get("status", {})
        replicas = spec.get("replicas", 1)
        ready = (controller_image(deployment) == expected and replicas >= 1
                 and status.get("observedGeneration", 0) >= deployment["metadata"].get("generation", 1)
                 and status.get("updatedReplicas", 0) == replicas
                 and status.get("availableReplicas", 0) >= replicas
                 and status.get("readyReplicas", 0) >= replicas)
        if ready:
            pods = kube.get("pods", namespace=BUILDER_NAMESPACE,
                            selector="app.kubernetes.io/name=zeabur-builder")["items"]
            for pod in pods:
                if pod.get("metadata", {}).get("deletionTimestamp"):
                    continue
                containers = pod.get("spec", {}).get("containers", [])
                statuses = pod.get("status", {}).get("containerStatuses", [])
                if (any(c.get("name") == "controller" and c.get("image") == expected for c in containers)
                        and any(c.get("name") == "controller" and c.get("ready") for c in statuses)):
                    return
        print("Waiting for the expected Builder image and ready replacement Pod...", flush=True)
        time.sleep(5)
    raise InstallError("The expected Builder image did not become ready before the timeout.")


def host_requirements():
    require(platform.system() == "Linux" and platform.machine() in ("x86_64", "amd64"),
            "Run this bundle on the new Linux amd64 server.")
    require(os.geteuid() == 0, "Run this installer as root on the new server.")
    require(shutil.which("k3s") is not None, "k3s is not installed or is not on PATH.")


def rollback(kube, directory):
    path = directory.resolve() / "state.json"
    require(path.is_file() and not path.is_symlink() and not (path.stat().st_mode & 0o077),
            "Rollback requires a private, regular backup state.json file.")
    state = json.loads(path.read_text())
    require(state.get("format") == 1 and state.get("node_ip") == EXPECTED_NODE_IP,
            "Backup does not identify this new cluster.")
    nodes = kube.get("nodes")["items"]
    require(len(nodes) == 1 and nodes[0]["metadata"]["name"] == state["node"],
            "Backup belongs to a different cluster.")
    require(any(address.get("type") == "InternalIP" and address.get("address") == EXPECTED_NODE_IP
                for address in nodes[0].get("status", {}).get("addresses", [])),
            "Backup node address does not match the current cluster.")
    current_config = kube.get("helmchartconfig", CHART_NAME, CHART_NAMESPACE, missing=True)
    require(state.get("helmchartconfig") is not None or current_config is None,
            "A new HelmChartConfig appeared after this backup; review it before rollback.")
    chart = clean_object(state["helmchart"])
    kube.apply(chart, dry_run=True)
    if state.get("helmchartconfig"):
        kube.apply(clean_object(state["helmchartconfig"]), dry_run=True)
        kube.apply(clean_object(state["helmchartconfig"]))
    kube.apply(chart)
    wait_controller(kube, state["deployment_image"])
    previous = {item["metadata"]["name"]: item for item in state["statefulsets"]}
    current = kube.get("statefulsets", namespace=BUILDER_NAMESPACE,
                       selector="app.kubernetes.io/name=buildkitd")["items"]
    for item in current:
        old = previous.get(item["metadata"]["name"])
        if old:
            restored = clean_object(old)
        else:
            # A newly created daemon has no older spec. Remove only this variant's six proxy
            # variables, preserving its volumes, registry TLS and every other setting.
            restored = clean_object(item)
            for container in restored["spec"]["template"]["spec"].get("containers", []):
                if "env" in container:
                    container["env"] = [env for env in container["env"] if env["name"] not in PROXY_NAMES]
        kube.apply(restored, dry_run=True)
        kube.apply(restored)
    print("ROLLBACK COMPLETE: the original Builder chart and controller are ready.")


def main(arguments=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="Validate and print the plan; default, no changes.")
    mode.add_argument("--apply", action="store_true", help="Stage verified images and update only the Builder HelmChart.")
    mode.add_argument("--rollback", type=Path, metavar="BACKUP", help="Restore a backup created by this installer.")
    parser.add_argument("--bundle", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--proxy", default="http://100.66.31.168:3128")
    parser.add_argument("--no-proxy", default=NO_PROXY)
    args = parser.parse_args(arguments)
    backup = None
    try:
        host_requirements()
        kube = Kube()
        if args.rollback:
            rollback(kube, args.rollback)
            return 0
        proxy, no_proxy = proxy_endpoint(args.proxy), bypass_list(args.no_proxy)
        root = args.bundle.resolve()
        manifest = verify_bundle(root)
        print("VERIFIED: all bundle files, chart and pinned image references.")
        state = guarded_cluster(kube, manifest)
        defaults = desired_defaults(root, manifest, proxy, no_proxy)
        check_overrides(kube, state, defaults)
        archive = rewritten_chart(bundle_path(root, manifest["chart"]), defaults)
        chart = desired_chart(state, manifest, archive)
        kube.apply(chart, dry_run=True)
        expected = next(image["digest_reference"] for image in manifest["images"] if image["role"] == "controller")
        print("Plan: {} -> {} on {} ({}).".format(ORIGINAL_IMAGE, expected, state["node"], EXPECTED_NODE_IP))
        print("Proxy: {}; internal cluster addresses bypass it.".format(proxy))
        print("Helm valuesContent, HelmChartConfig, registry settings and business workloads are preserved.")
        if not args.apply:
            print("CHECK COMPLETE: no files, images or cluster objects were changed. Use --apply to install.")
            return 0
        backup = backup_state(state, manifest)
        print("BACKUP: " + str(backup), flush=True)
        print("ROLLBACK COMMAND: python3 {} --rollback {}".format(Path(__file__).resolve(), backup), flush=True)
        stage_images(root, manifest)
        kube.apply(chart)
        wait_controller(kube, expected)
        print("BUILDER PROXY READY: expected maintained image is running. Retry the failed build in Zeabur.")
        return 0
    except (InstallError, OSError, ValueError, KeyError, TypeError, tarfile.TarError) as error:
        print("STOPPED: " + str(error), file=sys.stderr)
        if backup:
            print("Rollback remains available: python3 {} --rollback {}".format(Path(__file__).resolve(), backup),
                  file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Exercise K3s's real archive watcher, CRI pinning and retained-archive recovery.

Only bundle references and staged archives change in the disposable CI node.
There is no ctr import/pin fallback: evidence must describe the real watcher.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time

DIRECTORY = "/var/lib/rancher/k3s/agent/images"
PIN = "io.cri-containerd.pinned"
OWNER = "io.cattle.k3s.pinned"
SOCKET = "/run/k3s/containerd/containerd.sock"


def run(arguments, *, check=True, timeout=20):
    return subprocess.run(arguments, capture_output=True, text=True, check=check, timeout=timeout)


def archive_plan(root, manifest):
    """Group arbitrary image roles, with unique complete-archive destinations."""
    root = root.resolve()
    grouped = {}
    for image in manifest["images"]:
        relative = image["archive"]
        source = (root / relative).resolve()
        if not source.is_relative_to(root) or not source.is_file():
            raise ValueError("Bundle archive is missing or escapes its root")
        references = (image["tag"], image["digest_reference"])
        if any(not isinstance(ref, str) or not ref or ref.startswith("-") for ref in references):
            raise ValueError("Invalid bundle image reference")
        entry = grouped.setdefault(relative, {"source": source, "references": set(), "tags": set()})
        entry["references"].update(references)
        entry["tags"].add(image["tag"])
    if not grouped:
        raise ValueError("Bundle contains no images")
    result = []
    for relative, entry in sorted(grouped.items()):
        digest = hashlib.sha256()
        with entry["source"].open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
        path_digest = hashlib.sha256(relative.encode()).hexdigest()[:12]
        name = f"mingxing-proxy-{path_digest}-{digest.hexdigest()[:16]}.oci.tar"
        result.append(dict(entry, destination=DIRECTORY + "/" + name, size=entry["source"].stat().st_size))
    return result


def command_error(result):
    """An unavailable CLI or runtime is never evidence that an image is missing."""
    message = ((result.stderr or "") + "\n" + (result.stdout or "")).lower()
    if (result.returncode in (126, 127)
            or any(value in message for value in ("no help topic", "unknown command", "unknown flag",
                                                   "flag provided but not defined", "executable file not found"))):
        return "cli-error"
    if any(value in message for value in ("dial unix", "connection refused", "transport is closing",
                                          "failed to connect", "context deadline exceeded", "unavailable")):
        return "runtime-error"
    if "no such image" in message or re.search(r"\bimage\b[^\n]*(?:not found|notfound)", message):
        return "missing"
    return "inspect-error"


def cri_status(result):
    """Retain identity and pinning only, never verbose image configuration or environment."""
    if result.returncode:
        return {"state": command_error(result), "inspect_exit": result.returncode}
    try:
        status = json.loads(result.stdout)["status"]
        if not isinstance(status, dict):
            raise ValueError("status is not an object")
    except (ValueError, KeyError, TypeError):
        return {"state": "invalid-cri-response"}
    identity = status.get("id")
    if not isinstance(identity, str) or not identity:
        return {"state": "missing-image-id"}
    pinned = status.get("pinned") is True
    return {"state": "pinned" if pinned else "unpinned", "id": identity, "pinned": pinned}


def image_metadata(output, references):
    """ctr's last table column is LABELS; omit all unrelated references."""
    found = {}
    for line in output.splitlines():
        fields = line.split()
        if len(fields) >= 6 and fields[0] in references:
            labels = dict(item.split("=", 1) for item in fields[-1].split(",") if "=" in item)
            found[fields[0]] = {"digest": fields[2], "labels": labels}
    return found


class Verifier:
    def __init__(self, root, manifest, node, *, timeout=180, runner=run, socket=SOCKET):
        self.node = node
        self.timeout = timeout
        self.run = runner
        self.archives = archive_plan(root, manifest)
        self.references = {ref for entry in self.archives for ref in entry["references"]}
        self.artifacts = root.parent / "cluster/offline-import"
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.started = datetime.now(timezone.utc).isoformat()
        self.last_records = {}
        self.last_cache = {}
        self.socket = socket
        self.ctr_cli = None
        self.cri_cli = None
        self.cli_info = {}

    def execute(self, *arguments, check=True, timeout=20):
        return self.run(["docker", "exec", self.node, *arguments], check=check, timeout=timeout)

    def discover_tools(self):
        """Use actual binaries, since some K3s images do not expose `k3s ctr/crictl`."""
        for tool in ("ctr", "crictl"):
            located = self.execute("sh", "-c", 'command -v "$1"', "sh", tool, check=False)
            candidates = [located.stdout.strip()] if not located.returncode else []
            candidates.extend(("/bin/" + tool, "/usr/bin/" + tool, "/bin/aux/" + tool,
                               "/var/lib/rancher/k3s/data/current/bin/" + tool))
            attempts = []
            for path in dict.fromkeys(candidates):
                if not path.startswith("/") or "\n" in path:
                    continue
                exists = self.execute("test", "-x", path, check=False)
                if exists.returncode:
                    continue
                if tool == "ctr":
                    prefix = [path, "--address", self.socket]
                else:
                    endpoint = "unix://" + self.socket
                    prefix = [path, "--runtime-endpoint", endpoint, "--image-endpoint", endpoint]
                version = self.execute(*prefix, "version", check=False)
                if version.returncode:
                    attempts.append({"path": path, "exit": version.returncode,
                                     "state": command_error(version)})
                    continue
                self.cli_info[tool] = {"command": prefix, "version": version.stdout.strip()}
                if tool == "ctr":
                    self.ctr_cli = prefix
                else:
                    self.cri_cli = prefix
                break
            else:
                self.cli_info[tool] = {"state": "unavailable", "attempts": attempts}
                raise RuntimeError(f"Cannot initialize actual {tool} binary against {self.socket}: {attempts}")

    def stage(self):
        self.execute("mkdir", "-p", DIRECTORY)
        # K3s persists this cache only when it already exists; touch never truncates one.
        self.execute("touch", DIRECTORY + "/.cache.json")
        for entry in self.archives:
            destination = entry["destination"]
            # Publish only after docker cp has finished. The rename happens inside the
            # watched node, rather than exposing an incomplete archive at its final path.
            self.run(["docker", "cp", str(entry["source"]), self.node + ":" + destination + ".partial"],
                     timeout=60)
            self.execute("mv", "--", destination + ".partial", destination)

    def snapshot(self):
        if not self.ctr_cli or not self.cri_cli:
            raise RuntimeError("Containerd and CRI tools must pass discovery before inspection")
        listed = self.execute(*self.ctr_cli, "-n", "k8s.io", "images", "list", check=False)
        if listed.returncode:
            self.cli_info["metadata_query"] = {"state": command_error(listed), "exit": listed.returncode}
            raise RuntimeError("Containerd metadata query failed: " + command_error(listed))
        metadata = image_metadata(listed.stdout, self.references)
        records = {}
        for reference in sorted(self.references):
            inspected = self.execute(*self.cri_cli, "inspecti", "--quiet", reference, check=False, timeout=10)
            record = cri_status(inspected)
            record["containerd"] = metadata.get(reference)
            records[reference] = record
            self.last_records = records
            if record["state"] in ("cli-error", "runtime-error", "inspect-error", "invalid-cri-response"):
                raise RuntimeError(f"CRI inspection failed ({record['state']}): {reference}")
        return records

    @staticmethod
    def ready(record):
        labels = (record.get("containerd") or {}).get("labels", {})
        return (record.get("state") == "pinned"
                and labels.get(PIN) == "pinned" and labels.get(OWNER) == "pinned")

    def archive_cache(self):
        """A cache entry is written only after preloadFile, including retagging, succeeds."""
        result = {"ready": False, "entries": {}, "problems": {}}
        self.last_cache = result
        loaded = self.execute("cat", DIRECTORY + "/.cache.json", check=False)
        if loaded.returncode:
            result["problems"]["cache"] = "not-readable"
            return result
        try:
            cache = json.loads(loaded.stdout)
            if not isinstance(cache, dict):
                raise ValueError("cache is not an object")
        except (ValueError, TypeError):
            result["problems"]["cache"] = "invalid-or-incomplete-json"
            return result
        for entry in self.archives:
            path = entry["destination"]
            info = cache.get(path)  # K3s uses the full absolute watched path, not basename.
            if not isinstance(info, dict):
                result["problems"][path] = "entry-missing"
                continue
            result["entries"][path] = info
            tags = info.get("images")
            if not isinstance(tags, list) or not all(isinstance(tag, str) for tag in tags):
                result["problems"][path] = "invalid-image-list"
                continue
            if not entry["tags"].issubset(tags):
                result["problems"][path] = "expected-tag-missing"
                continue
            if type(info.get("size")) is not int or info["size"] != entry["size"]:
                result["problems"][path] = "cached-size-mismatch"
                continue
            observed = self.execute("stat", "-c", "%s %Y", path, check=False)
            try:
                size, modified = map(int, observed.stdout.split())
                timestamp = datetime.fromisoformat(info["modTime"].replace("Z", "+00:00"))
                if observed.returncode or timestamp.tzinfo is None:
                    raise ValueError("archive stat or cache timestamp is invalid")
                if size != entry["size"] or int(timestamp.timestamp()) != modified:
                    result["problems"][path] = "cache-stale-for-current-archive"
            except (ValueError, KeyError, TypeError, AttributeError):
                result["problems"][path] = "invalid-stat-or-cache-timestamp"
        result["ready"] = not result["problems"]
        return result

    def evidence(self, phase, records):
        result = {"phase": phase, "references": records, "cli": self.cli_info}
        report = self.artifacts / (phase + ".json")
        report.write_text(json.dumps(result, indent=2) + "\n")
        result["staged_archives"] = {}
        for entry in self.archives:
            stat = self.execute("stat", "-c", "%s %Y", entry["destination"], check=False)
            result["staged_archives"][entry["destination"]] = {
                "stat_exit": stat.returncode, "size_mtime": stat.stdout.strip()}
        result["cache"] = self.archive_cache()
        result["k3s_version"] = self.execute("k3s", "--version", check=False).stdout.strip()
        logs = self.run(["docker", "logs", "--timestamps", "--tail", "2000", "--since", self.started, self.node], check=False)
        selected = [line for line in (logs.stdout + logs.stderr).splitlines()
                    if re.search(r"import|pinned|image event|fileinfo cache", line, re.IGNORECASE)]
        (self.artifacts / (phase + ".node.log")).write_text("\n".join(selected[-1000:]) + "\n")
        report.write_text(json.dumps(result, indent=2) + "\n")

    def wait_pinned(self, phase):
        deadline = time.monotonic() + self.timeout
        while True:
            records = self.snapshot()
            cache = self.archive_cache()
            pending = {ref: value for ref, value in records.items() if not self.ready(value)}
            if not pending and cache["ready"]:
                self.evidence(phase, records)
                return
            if time.monotonic() >= deadline:
                self.evidence(phase, records)
                summary = ", ".join(
                    f"{ref}: {value['state']} (metadata={bool(value.get('containerd'))}, "
                    f"labels={(value.get('containerd') or {}).get('labels', {})})"
                    for ref, value in sorted(pending.items()))
                raise RuntimeError("K3s watcher import completion failed: " + summary
                                   + "; archive cache=" + json.dumps(cache["problems"], sort_keys=True))
            time.sleep(2)

    def remove_own_aliases(self):
        if not self.ctr_cli:
            raise RuntimeError("Containerd CLI must pass discovery before recovery")
        for reference in sorted(self.references):
            self.execute(*self.ctr_cli, "-n", "k8s.io", "images", "rm", reference)
        listed = self.execute(*self.ctr_cli, "-n", "k8s.io", "images", "list", "--quiet")
        if self.references.intersection(listed.stdout.splitlines()):
            raise RuntimeError("Bundle aliases remain before recovery test")

    def refresh_retained_archives(self):
        for entry in self.archives:
            destination = entry["destination"]
            previous = int(self.execute("stat", "-c", "%Y", destination).stdout.strip())
            # Strictly newer even if copied source mtime was in the future. BusyBox supports
            # POSIX touch -t; TZ applies to this command only, not the node configuration.
            stamp = datetime.fromtimestamp(max(int(time.time()), previous + 2), timezone.utc)
            self.run(["docker", "exec", "-e", "TZ=UTC", self.node, "touch", "-m", "-t",
                      stamp.strftime("%Y%m%d%H%M.%S"), destination])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("--node", default="k3d-zeabur-builder-server-0")
    parser.add_argument("--socket", default=SOCKET)
    args = parser.parse_args()
    root = args.bundle.resolve()
    manifest = json.loads((root / "bundle.json").read_text())
    verifier = Verifier(root, manifest, args.node, socket=args.socket)
    try:
        verifier.discover_tools()
        verifier.stage()
        verifier.wait_pinned("imported")
        verifier.remove_own_aliases()
        verifier.refresh_retained_archives()
        verifier.wait_pinned("recovered")
    except (OSError, RuntimeError, subprocess.SubprocessError):
        try:
            verifier.evidence("failure", verifier.last_records)
        except (OSError, subprocess.SubprocessError) as error:
            print("Additional import diagnostics unavailable:", type(error).__name__)
        raise
    print("OCI_CRI_OK: K3s watcher imported/pinned all roles and recovered every retained-archive alias.")


if __name__ == "__main__":
    main()

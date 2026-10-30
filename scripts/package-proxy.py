#!/usr/bin/env python3
"""Create a content-verified amd64 OCI bundle for the MingXing Builder patch."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import tarfile


def sha256(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n")


def checked_blob(source, descriptor, media_types):
    """Validate an OCI reference before deriving a filesystem path from it."""
    if not isinstance(descriptor, dict) or descriptor.get("mediaType") not in media_types:
        raise ValueError("Unexpected OCI descriptor media type")
    digest = descriptor.get("digest", "")
    if not isinstance(digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
        raise ValueError("OCI descriptors must use canonical SHA256 digests")
    size = descriptor.get("size")
    if type(size) is not int or size < 0:
        raise ValueError("Invalid OCI descriptor size")
    path = source / "blobs/sha256" / digest.split(":")[1]
    if not path.is_file() or path.is_symlink():
        raise ValueError("A referenced OCI blob is missing or unsafe")
    if path.stat().st_size != size or sha256(path) != digest.split(":")[1]:
        raise ValueError("OCI descriptor content or size does not match")
    return path


def merge_image(source, destination, ref):
    if source.is_symlink() or (source / "blobs").is_symlink() or (source / "blobs/sha256").is_symlink():
        raise ValueError("OCI source paths must not be symbolic links")
    if json.loads((source / "oci-layout").read_text()).get("imageLayoutVersion") != "1.0.0":
        raise ValueError("Unsupported OCI layout version")
    index = json.loads((source / "index.json").read_text())
    if index.get("schemaVersion") != 2:
        raise ValueError("Unsupported OCI index schema")
    entries = index.get("manifests", [])
    if len(entries) != 1:
        raise ValueError("Expected one amd64 image manifest per source layout")
    descriptor = entries[0]
    manifest_file = checked_blob(source, descriptor, {
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    })
    digest = descriptor["digest"]
    manifest = json.loads(manifest_file.read_text())
    if manifest.get("schemaVersion") != 2:
        raise ValueError("Unsupported OCI manifest schema")
    config_file = checked_blob(source, manifest.get("config"), {
        "application/vnd.oci.image.config.v1+json",
        "application/vnd.docker.container.image.v1+json",
    })
    layers = manifest.get("layers")
    if not isinstance(layers, list):
        raise ValueError("OCI manifest layers must be a list")
    for layer in layers:
        checked_blob(source, layer, {
            "application/vnd.oci.image.layer.v1.tar",
            "application/vnd.oci.image.layer.v1.tar+gzip",
            "application/vnd.oci.image.layer.v1.tar+zstd",
            "application/vnd.docker.image.rootfs.diff.tar.gzip",
        })
    config = json.loads(config_file.read_text())
    if config.get("architecture") != "amd64" or config.get("os") != "linux":
        raise ValueError("Only Linux amd64 images may enter this installation bundle")
    for blob in (source / "blobs/sha256").iterdir():
        if blob.is_symlink() or not blob.is_file() or sha256(blob) != blob.name:
            raise ValueError("Invalid OCI content")
        target = destination / "blobs/sha256" / blob.name
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.exists():
            shutil.copyfile(blob, target)
    repository = ref.rsplit(":", 1)[0]
    digest_reference = repository + "@" + digest
    imported = []
    for name in (ref, digest_reference):
        entry = dict(descriptor)
        entry["annotations"] = {"org.opencontainers.image.ref.name": name}
        imported.append(entry)
    return imported, {"tag": ref, "digest_reference": digest_reference,
                      "archive": "images/components-amd64.oci.tar"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--images", type=Path, required=True)
    parser.add_argument("--controller-ref", required=True)
    parser.add_argument("--job-ref", required=True)
    parser.add_argument("--chart", type=Path, required=True)
    parser.add_argument("--defaults", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if len(args.commit) != 40 or any(c not in "0123456789abcdef" for c in args.commit):
        parser.error("A full source commit is required")
    root = Path(__file__).resolve().parent.parent
    name = "zeabur-builder-proxy-" + args.version + "-linux-amd64"
    bundle = args.output / name
    if bundle.exists():
        raise ValueError("Refusing to overwrite an existing bundle")
    bundle.mkdir(parents=True)
    layout = args.output.parent / "proxy-merged-oci"
    layout.mkdir(parents=True, exist_ok=False)
    manifests, images = [], []
    for image, ref in (("builder", args.controller_ref), ("builder-job", args.job_ref)):
        entries, record = merge_image(args.images / image, layout, ref)
        record["role"] = "controller" if image == "builder" else "job"
        manifests.extend(entries)
        images.append(record)
    write_json(layout / "index.json", {"schemaVersion": 2, "manifests": manifests})
    write_json(layout / "oci-layout", {"imageLayoutVersion": "1.0.0"})
    archive = bundle / "images/components-amd64.oci.tar"
    archive.parent.mkdir()
    with tarfile.open(archive, "w") as tar:
        for path in sorted(layout.rglob("*")):
            if path.is_file():
                tar.add(path, arcname=path.relative_to(layout), recursive=False)
    (bundle / "products").mkdir()
    chart = bundle / "products/zeabur-builder-proxy.tgz"
    shutil.copyfile(args.chart, chart)
    defaults = json.loads(args.defaults.read_text())
    # The source chart intentionally has empty release digests. Keep the already installed
    # official planner content-pinned while replacing only the controller and Job binary.
    defaults["railpackImage"] = {
        "repository": "ghcr.io/zeabur-oss/builder-railpack", "tag": "",
        "digest": "sha256:db71c5e8c5f6cb17578e75ba7d0ff03abc7da0b1069253b8e7aa8058404dd77c",
    }
    write_json(bundle / "products/default-values.json", defaults)
    shutil.copyfile(root / "deploy/proxy/install.py", bundle / "install.py")
    shutil.copyfile(root / "LICENSE", bundle / "LICENSE")
    write_json(bundle / "bundle.json", {
        "version": args.version, "source_commit": args.commit,
        "upstream_commit": "8fd8b6e9b6480cb2ea3fb28d250b4b71da663b8a",
        "source_repository": "https://github.com/011011100/zeabur-builder-proxy",
        "chart": "products/zeabur-builder-proxy.tgz", "chart_sha256": sha256(chart),
        "defaults": "products/default-values.json", "images": images,
    })
    (bundle / "使用说明.txt").write_text(
        "这是基于官方 Builder v0.1.1 的 MingXing 代理兼容版本，不是官方发布镜像。\n"
        "在新服务器解压后，先运行只读检查，再应用；保留输出中的备份目录用于回退。\n"
        "python3 install.py --proxy http://PROXY_IP:3128 --check\n"
        "python3 install.py --proxy http://PROXY_IP:3128 --apply\n"
        "python3 install.py --rollback BACKUP_DIRECTORY\n"
        "安装只更新 Builder，保留 GitHub App、项目、现有运行服务和数据。\n"
        "检查 Build/BuildRun 的新状态以验证代理、仓库权限和构建依赖；入口就绪不等于构建已通过。\n"
    )
    lines = [sha256(path) + "  " + str(path.relative_to(bundle))
             for path in sorted(bundle.rglob("*")) if path.is_file()]
    (bundle / "SHA256SUMS").write_text("\n".join(lines) + "\n")
    args.output.mkdir(parents=True, exist_ok=True)
    target = args.output / (name + ".tar.gz")
    with tarfile.open(target, "w:gz", compresslevel=4) as tar:
        tar.add(bundle, arcname=name)
    (args.output / (target.name + ".sha256")).write_text(sha256(target) + "  " + target.name + "\n")
    print("PACKAGED:", target)


if __name__ == "__main__":
    main()

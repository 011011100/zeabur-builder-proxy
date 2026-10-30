#!/usr/bin/env python3
"""Synthetic Git/probe hosts plus a CONNECT relay, restricted to disposable CI."""

import http.server
import ipaddress
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import urllib.parse
import urllib.request


PROXY_NAMES = {"HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy"}


def direct_opener():
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


def inspect_image(reference):
    authority, repository = reference.split("/", 1)
    repository, digest = repository.rsplit("@", 1)
    base = "http://" + authority + "/v2/" + repository
    request = urllib.request.Request(base + "/manifests/" + digest, headers={
        "Accept": "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json",
    })
    with direct_opener().open(request, timeout=15) as response:
        manifest = json.load(response)
    with direct_opener().open(base + "/blobs/" + manifest["config"]["digest"], timeout=15) as response:
        config = json.load(response)
    image_env = config.get("config", {}).get("Env", []) or []
    names = {entry.split("=", 1)[0] for entry in image_env}
    if names & PROXY_NAMES:
        raise SystemExit("proxy variables were persisted in final image environment")
    print("IMAGE_ENV_OK", reference, flush=True)


def repositories(directory=Path("/tmp")):
    root = directory / "git"
    root.mkdir()
    work = directory / "work"
    work.mkdir()
    dockerfile = """FROM busybox:1.37.0
RUN test -n "$http_proxy" && test "$http_proxy" = "$HTTP_PROXY" && \\
    wget -T 15 -qO /proxy-check 'http://probe.proxy-test.invalid/check?phase=dockerfile' && \\
    grep -q proxy-smoke-ok /proxy-check
CMD ["cat", "/proxy-check"]
"""
    command = ("sh -c 'test -n \"$http_proxy\" && test \"$http_proxy\" = \"$HTTP_PROXY\" "
               "&& test -n \"$no_proxy\" && mkdir -p /app "
               "&& wget -T 15 -qO /app/proxy-check http://probe.proxy-test.invalid/check?phase=railpack "
               "&& grep -q proxy-smoke-ok /app/proxy-check'")
    plan = {
        "steps": [{"name": "build", "inputs": [{"image": "busybox:1.37.0"}],
                   "commands": [{"cmd": command}]}],
        "deploy": {"base": {"image": "busybox:1.37.0"},
                   "inputs": [{"step": "build", "include": ["/app/proxy-check"]}],
                   "startCommand": "cat /app/proxy-check"},
    }
    (work / "Dockerfile").write_text(dockerfile)
    (work / "railpack-plan.json").write_text(json.dumps(plan))
    git_env = {**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
               "HOME": str(directory),
               "GIT_AUTHOR_NAME": "Proxy CI", "GIT_AUTHOR_EMAIL": "ci@example.invalid",
               "GIT_COMMITTER_NAME": "Proxy CI", "GIT_COMMITTER_EMAIL": "ci@example.invalid"}
    for arguments in (("init", "-q", "-b", "main"), ("add", "."),
                      ("commit", "-qm", "Synthetic proxy build fixture")):
        subprocess.run(["git", "-C", str(work), *arguments], env=git_env, check=True)
    subprocess.run(["git", "clone", "-q", "--bare", str(work), str(root / "repo.git")],
                   env=git_env, check=True)
    return root


class Proxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    git_root = None
    git_backend = None

    def log_message(self, *_arguments):
        pass

    def record(self, host, path):
        print(json.dumps({"method": self.command, "host": host, "path": path,
                          "user_agent": self.headers.get("User-Agent", "")}), flush=True)

    def answer(self, status, body, headers=None):
        self.send_response(status)
        for name, value in headers or []:
            if name.lower() not in {"content-length", "connection", "transfer-encoding"}:
                self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def do_GET(self):
        self.request_http()

    def do_POST(self):
        self.request_http()

    def request_http(self):
        target = urllib.parse.urlsplit(self.path)
        if not target.hostname:
            # Kubelet readiness probes use origin-form and do not contact synthetic domains.
            if target.path == "/health":
                self.answer(200, b"ok\n")
            else:
                self.answer(400, b"an absolute proxy request is required\n")
            return
        self.record(target.hostname, target.path + ("?" + target.query if target.query else ""))
        if target.hostname == "probe.proxy-test.invalid" and target.path == "/check":
            self.answer(200, b"proxy-smoke-ok\n")
            return
        if target.hostname != "git.proxy-test.invalid" or not target.path.startswith("/repo.git/"):
            self.answer(403, b"only synthetic HTTP targets are allowed\n")
            return
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 <= length <= 1024 * 1024:
            self.answer(413, b"test Git body too large\n")
            return
        body = self.rfile.read(length)
        environment = {**os.environ, "GIT_PROJECT_ROOT": str(self.git_root),
                       "GIT_HTTP_EXPORT_ALL": "1", "REQUEST_METHOD": self.command,
                       "PATH_INFO": target.path, "QUERY_STRING": target.query,
                       "CONTENT_TYPE": self.headers.get("Content-Type", ""),
                       "CONTENT_LENGTH": str(len(body)), "REMOTE_ADDR": self.client_address[0],
                       "GIT_PROTOCOL": self.headers.get("Git-Protocol", "")}
        result = subprocess.run([self.git_backend], input=body, capture_output=True,
                                env=environment, timeout=20, check=False)
        raw_headers, separator, payload = result.stdout.partition(b"\r\n\r\n")
        if not separator or result.returncode:
            self.answer(502, b"test git-http-backend failed\n")
            return
        status, headers = 200, []
        for raw in raw_headers.decode("latin-1").split("\r\n"):
            name, value = raw.split(":", 1)
            if name.lower() == "status":
                status = int(value.strip().split()[0])
            else:
                headers.append((name, value.strip()))
        self.answer(status, payload, headers)

    def do_CONNECT(self):
        target = urllib.parse.urlsplit("//" + self.path)
        self.record(target.hostname, self.path)
        if target.port != 443:
            self.answer(403, b"CI relay allows only public HTTPS\n")
            return
        addresses = socket.getaddrinfo(target.hostname, target.port, type=socket.SOCK_STREAM)
        if not addresses or any(not ipaddress.ip_address(item[4][0]).is_global for item in addresses):
            self.answer(403, b"CI relay refuses private or special addresses\n")
            return
        try:
            upstream = socket.create_connection((target.hostname, target.port), timeout=15)
        except OSError:
            self.answer(502, b"CI relay could not reach upstream\n")
            return
        with upstream:
            self.send_response(200, "Connection established")
            self.end_headers()
            self.wfile.flush()
            sockets = [self.connection, upstream]
            while True:
                ready, _, _ = select.select(sockets, [], [], 30)
                if not ready:
                    break
                for source in ready:
                    data = source.recv(64 * 1024)
                    if not data:
                        self.close_connection = True
                        return
                    destination = upstream if source is self.connection else self.connection
                    destination.sendall(data)
        self.close_connection = True


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "inspect":
        inspect_image(sys.argv[2])
        return
    if len(sys.argv) == 2 and sys.argv[1] == "direct-check":
        for host in ("git.proxy-test.invalid", "probe.proxy-test.invalid"):
            try:
                socket.getaddrinfo(host, 80)
            except socket.gaierror:
                continue
            raise SystemExit("synthetic target unexpectedly resolves without the proxy")
        print("DIRECT_TARGETS_UNRESOLVABLE", flush=True)
        return
    Proxy.git_root = repositories()
    Proxy.git_backend = subprocess.check_output(["git", "--exec-path"], text=True).strip() + "/git-http-backend"
    http.server.ThreadingHTTPServer(("0.0.0.0", 3128), Proxy).serve_forever()


if __name__ == "__main__":
    main()

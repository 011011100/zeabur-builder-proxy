# Security policy

Please report vulnerabilities privately through GitHub's "Report a vulnerability" (private
vulnerability reporting) on this repository. Do not open public issues for security problems.

## Support

Zeabur Builder has no release yet. Until it does, fixes land on `main` only.

## Trust model

The unit of isolation is the namespace. Teams that share a cluster cannot read each other's
source, secrets or build cache, and cannot push to each other's images:

- Each build namespace has its own buildkitd and cache, its own certificate authority and client
  certificate. With `registry.tokenKey`, each run pushes with a short-lived token for its own
  `Build`'s repository only; with `registry.authPrefix`, with a namespace account that pushes
  only below `<registry.host>/<namespace>/`.
- An archive source is read only from `<registry.host>/<namespace>/<build>/source`, with the
  run's registry credentials. The registry decides who else may read it: the Zeabur Catalog's
  Zot lets only a token for that repository read it; the test registry shows every upload to
  anyone who can reach it. The Job unpacks the archive without writing outside the source folder.
- Whoever reads the registry token key can push to and delete any repository. Only the
  controller may read it.
- Builds in one namespace trust each other. They share a buildkitd, so a `RUN` step can read the
  cache mounts of other builds in the same namespace.
- Each `RUN` step has its own network namespace. It reaches the internet and the registry; the
  egress policy in `deploy/chart/templates/buildkitd.yaml` keeps it from Pods, Services, nodes and the
  cloud metadata endpoint (IPv4).
- buildkitd runs privileged in a user namespace (`hostUsers: false`), so root in a build maps to
  an unprivileged user on the node. This stops cross-team access. It does not stop an attacker
  who escapes the container through a kernel bug.

For code from people you do not trust, the namespace boundary is too weak.
`--buildkitd-runtime-class` sets buildkitd's `runtimeClassName`, for a sandbox runtime such as
gVisor or Kata on nodes of its own.

Maintainers must enable GitHub private vulnerability reporting and confirm that the reporting
link is available before making a release public.

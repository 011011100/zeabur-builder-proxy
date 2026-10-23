# Contributing

## Developer Certificate of Origin

This project uses the [Developer Certificate of Origin 1.1](DCO), not a CLA. Sign off every
commit to certify that you wrote it or otherwise have the right to submit it under the project
licence:

```sh
git commit -s
```

CI rejects pull requests whose commits lack a `Signed-off-by` line matching the author. Commit
messages follow [Conventional Commits](https://www.conventionalcommits.org/).

## Before you open a pull request

Run what CI runs, inside `nix develop`:

```sh
cargo fmt --all --check && nixfmt --check flake.nix && shellcheck scripts/*.sh && actionlint
cargo lint                                   # clippy, all targets and features, -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items
cargo nextest run --workspace
cargo deny --locked check
./scripts/check-levels.sh
./scripts/check-chart.sh
```

Generated files are committed and checked by tests. Regenerate them with:

```sh
cargo run -q -p zeabur-builder -- crds > deploy/crds.json
ZEABUR_BUILDER_BLESS=1 cargo nextest run -p zeabur-builder-jobspec   # build Job golden files
```

Review a golden-file diff like code: it is exactly what will run in every build.

Design rationale lives next to the code, in rustdoc: state the decision and its reason, and
cite the specification, upstream source or prior art it rests on. Never cite something you have
not read; write "not verified" instead. Documentation is written in English; the plan is in
Traditional Chinese.

## Crate levels

A crate may depend only on crates of a lower level (`[workspace.metadata.levels]` in
`Cargo.toml`, checked by `scripts/check-levels.sh`):

| Level | Crate | Holds |
|---|---|---|
| 0 | `zeabur-builder-core` | Repository keys, tags, routing, build results |
| 1 | `zeabur-builder-api` | The custom resources |
| 2 | `zeabur-builder-jobspec` | The build Job |
| 3 | `zeabur-builder` | The binary |

Levels 0 to 2 are pure: no I/O, no clock, no randomness. The controller's decisions are
functions of their inputs, so they are tested without a cluster.

## Toolchain, lints and dependencies

Shared with [Zeabur Identity](https://github.com/zeabur/identity):

- `rust-toolchain.toml` is the only place the compiler version is set; `scripts/bump-toolchain.sh`
  also updates the hash in `flake.nix`.
- Lints are set once in `Cargo.toml`. All are `warn`, and `cargo lint` turns them into errors.
  Suppress with `#[expect(lint, reason = "…")]`. Only the binary's `output` module writes to
  stdout or stderr.
- `cargo deny check` enforces `deny.toml`: OSI-approved licences, crates.io only, aws-lc-rs as
  the only cryptographic backend, no advisories.
- Cargo does not pick a release younger than seven days (`.cargo/config.toml`).

## Tests against a cluster

Use a disposable cluster with its own kubeconfig, never your normal context:

```sh
export KUBECONFIG=$(mktemp -d)/kubeconfig
kind create cluster --name zeabur-builder
cargo run -q -p zeabur-builder -- crds | kubectl apply -f -
kind delete cluster --name zeabur-builder
```

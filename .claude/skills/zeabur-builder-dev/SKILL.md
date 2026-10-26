---
name: zeabur-builder-dev
description: Development loop for the Zeabur Builder repository — the checks CI runs and the engineering standards for comments, commits, tests, design and scale. Use before changing any Rust, chart, Nix or CI file in this repo.
---

# Zeabur Builder development loop

Crate levels, toolchain, generated files and cluster tests are in `CONTRIBUTING.md`.

## Checks (what CI runs)
```sh
nix develop -c bash -c 'cargo fmt --all --check && nixfmt --check flake.nix && shellcheck scripts/*.sh && actionlint'
nix develop -c cargo lint
RUSTDOCFLAGS="-D warnings" nix develop -c cargo doc --workspace --no-deps --document-private-items --locked
nix develop -c cargo nextest run --workspace --all-features --locked --no-tests=warn
nix develop -c cargo deny --locked check
nix develop -c ./scripts/check-levels.sh
nix develop -c ./scripts/check-chart.sh
```

## Standards
- Writing a comment, doc comment or commit message; adding or removing a test; adding an
  abstraction, option or flag; or code that lists, polls, stores or fans out: apply
  [`STANDARDS.md`](STANDARDS.md) and check the change against it.

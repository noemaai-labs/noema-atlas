# Contributing to Noema Atlas

Noema Atlas is an Apache-2.0 project for verified distribution of local LLM
artifacts. Contributions are welcome across the engine, CLI, desktop apps,
packaging, documentation, tests, and security hardening.

## Start here

Before opening a large pull request, open an issue or discussion with the shape
of the change. Small bug fixes, documentation corrections, tests, and focused UI
polish can go straight to a pull request.

Project roles and maintainer decision-making are described in
[GOVERNANCE.md](GOVERNANCE.md).

Good first contributions usually fit one of these buckets:

- A reproducible bug with a narrow fix and a regression test.
- A documentation update that matches the current commands and workflows.
- A cross-platform packaging improvement that keeps release behavior consistent.
- A small UI fix in `crates/desktop` or `crates/studio` with screenshots.
- A security or verification hardening change coordinated through
  [SECURITY.md](SECURITY.md) when it affects vulnerability disclosure.

## Project principles

- Verify bytes, not routes. Downloads and peer transfers must keep content
  identity, manifest signatures, and hash checks central to the design.
- Keep private or gated content private by default. Public seeding must require
  clear user intent when licensing or privacy is uncertain.
- Preserve cross-platform behavior. Linux, macOS, and Windows should stay first
  class for the root workspace; Studio has its own toolchain requirements.
- Prefer small, reviewable changes. Large rewrites need a design issue first.
- Do not commit model weights, private manifests, access tokens, certificates,
  generated installers, or local cache data.

## Development setup

Install a recent Rust toolchain. The root workspace declares an MSRV of Rust
1.82, while the CI uses stable Rust. Noema Studio is deliberately separate and
requires Rust 1.88 or newer because of Tauri.

Useful commands from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo build -p noema-core -p noema-cli --features iroh
```

Linux desktop builds also need GUI development packages. The CI workflow lists
the current Ubuntu packages in `.github/workflows/ci.yml`.

Studio development:

```sh
cd crates/studio
npm --prefix ui ci
cargo tauri dev
```

See [crates/studio/README.md](crates/studio/README.md) for Studio-specific
details, including the standalone workspace and Tauri workflow.

## Pull request expectations

- Keep the pull request focused on one behavior change or one documentation
  topic.
- Describe the user-visible effect, the verification performed, and any known
  trade-offs.
- Add or update tests for behavior changes. If tests are not practical, explain
  why in the pull request.
- Run formatting before opening the pull request.
- Include screenshots or short recordings for desktop UI changes.
- Update docs when changing commands, release behavior, security assumptions, or
  contributor workflow.
- Keep dependency updates intentional. Commit the relevant lockfile changes and
  explain why the update is needed.
- Never include secrets, tokens, signing materials, private tracker URLs, or
  large model artifacts.

Noema Atlas uses an inbound-equals-outbound contribution policy: by submitting a
contribution, you certify that you have the right to submit it and that it may be
distributed under the repository license, Apache-2.0.

## Issue triage

When filing a bug, include:

- OS and version.
- Noema Atlas version or commit.
- Which surface is affected: core, CLI, native Atlas, Studio, packaging,
  registry, or documentation.
- Exact reproduction steps.
- Expected and actual behavior.
- Relevant logs with secrets and tokens removed.

Do not report suspected vulnerabilities in public issues. Use the private
reporting path in [SECURITY.md](SECURITY.md).

## Review standards

Maintainers review for correctness, cross-platform behavior, privacy defaults,
security impact, and maintainability. A change that touches the downloader,
manifest verifier, peer transport, cache, install/delete paths, or secret storage
may need extra tests or a threat-model note before it merges.

## Releases

Release packaging is documented in [docs/releasing.md](docs/releasing.md).
Release workflows run from tags and produce installers for macOS, Windows, and
Linux when the required signing secrets are configured.

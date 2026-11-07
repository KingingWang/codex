# Codex fork npm distribution

This directory builds a personal npm distribution of the Codex CLI:

- `@kingingwang/codex`: the main package and `codex` launcher
- `@kingingwang/codex-<platform>`: the canonical Codex package for one supported platform

The main package uses `optionalDependencies`, so npm normally installs only the binary package matching the current operating system and architecture.

Each platform package embeds the canonical Codex package layout produced by
`scripts/build_codex_package.py`:

```text
codex-package.json
bin/codex
bin/codex-code-mode-host
codex-resources/   # bwrap on Linux, sandbox helpers on Windows, zsh on Unix
codex-path/rg
```

The full layout matters: the app-server daemon validates it on first start and
seeds its managed installation from it. A bare `codex` binary fails with
"this CLI has no complete local package". The release workflows attach the
same archives (`codex-package-<target>.tar.gz`) to the GitHub release as well.

## Packages

| Platform      | Package                           |
| ------------- | --------------------------------- |
| Linux x64     | `@kingingwang/codex-linux-x64`    |
| Linux ARM64   | `@kingingwang/codex-linux-arm64`  |
| macOS x64     | `@kingingwang/codex-darwin-x64`   |
| macOS ARM64   | `@kingingwang/codex-darwin-arm64` |
| Windows x64   | `@kingingwang/codex-win32-x64`    |
| Windows ARM64 | `@kingingwang/codex-win32-arm64`  |

The workflow downloads artifacts from successful runs of all three simple release workflows for one commit. It does **not** combine latest releases from different commits.

## Local assembly

Create a directory with one subdirectory per Actions artifact:

```text
release-assets/
  codex-package-x86_64-unknown-linux-musl/codex-package-x86_64-unknown-linux-musl.tar.gz
  codex-package-aarch64-unknown-linux-musl/codex-package-aarch64-unknown-linux-musl.tar.gz
  codex-package-x86_64-apple-darwin/codex-package-x86_64-apple-darwin.tar.gz
  codex-package-aarch64-apple-darwin/codex-package-aarch64-apple-darwin.tar.gz
  codex-package-x86_64-pc-windows-msvc/codex-package-x86_64-pc-windows-msvc.tar.gz
  codex-package-aarch64-pc-windows-msvc/codex-package-aarch64-pc-windows-msvc.tar.gz
```

Then assemble and validate the seven packages:

```sh
node npm-dist/scripts/assemble.mjs \
  --artifacts-dir release-assets \
  --version 0.147.0-fork.20260812115120
```

The version should use the `codex-rs` Cargo version plus a unique fork suffix. The workflow derives this deterministically from the source commit timestamp.

## Local dry run

After assembly, inspect the exact publish commands without touching npm:

```sh
node npm-dist/scripts/publish.mjs --dry-run
```

## Publish

The CI workflow publishes with the repository secret `NPM_TOKEN`.

For local authenticated publishing, run:

```sh
node npm-dist/scripts/publish.mjs
```

Platform packages publish first and the main package publishes last. Versions already present in the registry are skipped, so a failed run can be retried at the same version.

## GitHub Actions setup

Create repository variable `CODEX_NPM_PUBLISH=true`. Automatic `workflow_run` publication stays disabled until that variable exists, while manual workflow dispatch remains available for controlled first-time publishing.

## Install

```sh
npm uninstall -g @openai/codex
npm install -g @kingingwang/codex
codex --version
```

The uninstall step avoids a global `codex` command conflict with the official package.

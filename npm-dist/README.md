# Codex fork npm distribution

This directory builds a personal npm distribution of the Codex CLI:

- `@kingingwang/codex`: the main package and `codex` launcher
- `@kingingwang/codex-<platform>`: the native binary for one supported platform

The main package uses `optionalDependencies`, so npm normally installs only the binary package matching the current operating system and architecture.

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
  codex-x86_64-musl/codex-linux-x86_64-musl
  codex-aarch64-musl/codex-linux-aarch64-musl
  codex-macos-x86_64/codex-macos-x86_64.zst
  codex-macos-aarch64/codex-macos-aarch64.zst
  codex-windows-x86_64/codex-windows-x86_64.exe
  codex-windows-aarch64/codex-windows-aarch64.exe
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

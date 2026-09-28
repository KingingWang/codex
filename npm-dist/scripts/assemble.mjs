#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const distRoot = resolve(scriptDir, "..");
const repoRoot = resolve(distRoot, "..");
const templatesDir = join(distRoot, "templates");

export const platforms = [
  {
    packageName: "codex-linux-x64",
    target: "x86_64-unknown-linux-musl",
    os: ["linux"],
    cpu: ["x64"],
  },
  {
    packageName: "codex-linux-arm64",
    target: "aarch64-unknown-linux-musl",
    os: ["linux"],
    cpu: ["arm64"],
  },
  {
    packageName: "codex-darwin-x64",
    target: "x86_64-apple-darwin",
    os: ["darwin"],
    cpu: ["x64"],
  },
  {
    packageName: "codex-darwin-arm64",
    target: "aarch64-apple-darwin",
    os: ["darwin"],
    cpu: ["arm64"],
  },
  {
    packageName: "codex-win32-x64",
    target: "x86_64-pc-windows-msvc",
    os: ["win32"],
    cpu: ["x64"],
  },
  {
    packageName: "codex-win32-arm64",
    target: "aarch64-pc-windows-msvc",
    os: ["win32"],
    cpu: ["arm64"],
  },
];

function parseArgs(argv) {
  const args = {
    artifactsDir: join(repoRoot, "release-assets"),
    outDir: join(distRoot, "publish"),
    version: undefined,
    scope: process.env.CODEX_NPM_SCOPE || "@kingingwang",
    repo: process.env.CODEX_DIST_REPO || "KingingWang/codex",
  };

  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    const value = argv[index + 1];
    if (arg === "--artifacts-dir" || arg === "--out" || arg === "--version") {
      if (!value) throw new Error(`${arg} requires a value`);
      if (arg === "--artifacts-dir") args.artifactsDir = value;
      if (arg === "--out") args.outDir = value;
      if (arg === "--version") args.version = value;
      index++;
    } else if (arg === "--scope") {
      if (!value) throw new Error("--scope requires a value");
      args.scope = value;
      index++;
    } else if (arg === "--repo") {
      if (!value) throw new Error("--repo requires a value");
      args.repo = value;
      index++;
    } else if (arg === "--help") {
      console.log(usage());
      process.exit(0);
    } else {
      throw new Error(`Unknown option: ${arg}`);
    }
  }
  return args;
}

function usage() {
  return `Usage: node npm-dist/scripts/assemble.mjs [options]

Options:
  --artifacts-dir <dir>  Directory containing per-artifact subdirectories with
                         codex-package-<target>.tar.gz archives
  --out <dir>            Output directory (default: npm-dist/publish)
  --version <version>    Exact npm version; defaults to Cargo version plus -fork.<timestamp>
  --scope <scope>        npm scope (default: @kingingwang)
  --repo <owner/name>    GitHub repository used in package metadata
  --help                 Show this help

Environment:
  CODEX_NPM_SCOPE        Default npm scope
  CODEX_DIST_REPO        Default GitHub repository
  SOURCE_DATE_EPOCH      Build timestamp used for the default version`;
}

export function cargoVersion(
  cargoToml = join(repoRoot, "codex-rs", "Cargo.toml"),
) {
  const match = readFileSync(cargoToml, "utf8").match(
    /^version\s*=\s*"([^"]+)"/m,
  );
  if (!match) {
    throw new Error("Unable to read version from codex-rs/Cargo.toml");
  }
  return match[1];
}

export function defaultVersion(
  baseVersion,
  sourceDateEpoch = process.env.SOURCE_DATE_EPOCH,
) {
  const timestamp = new Date(
    sourceDateEpoch ? Number.parseInt(sourceDateEpoch, 10) * 1000 : Date.now(),
  );
  if (Number.isNaN(timestamp.getTime())) {
    throw new Error(`Invalid SOURCE_DATE_EPOCH: ${sourceDateEpoch}`);
  }

  const stamp = timestamp
    .toISOString()
    .replace(/[-:TZ.]/g, "")
    .slice(0, 14);
  return `${baseVersion}-fork.${stamp}`;
}

export function validateScope(scope) {
  if (!/^@[a-z0-9][a-z0-9-._]*$/i.test(scope)) {
    throw new Error(`Invalid npm scope: ${scope}`);
  }
  return scope;
}

export function validateRepository(repository) {
  if (!/^[\w.-]+\/[\w.-]+$/.test(repository)) {
    throw new Error(`Invalid GitHub repository: ${repository}`);
  }
  return repository;
}

export function validateVersion(version) {
  const pattern =
    /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
  if (!pattern.test(version)) {
    throw new Error(`Invalid semantic version: ${version}`);
  }
  return version;
}

function run(command, args, options = {}) {
  const executable =
    process.platform === "win32" && command === "npm" ? "npm.cmd" : command;
  const result = spawnSync(executable, args, {
    cwd: options.cwd,
    encoding: "utf8",
    stdio: options.capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    const output = [result.stdout, result.stderr].filter(Boolean).join("\n");
    throw new Error(output || `Command failed: ${command} ${args.join(" ")}`);
  }
  return result;
}

// The file list mirrors the daemon seeding validation in
// codex-rs/app-server-daemon/src/prepare_install.rs (validate_package). A
// bare CLI binary is not enough: first daemon start copies this layout into
// CODEX_HOME and refuses packages missing any of these entries.
function requiredPackageFiles(platform) {
  const isWindows = platform.os[0] === "win32";
  const exe = isWindows ? ".exe" : "";
  const files = [
    "codex-package.json",
    `bin/codex${exe}`,
    `bin/codex-code-mode-host${exe}`,
    `codex-path/rg${exe}`,
  ];
  if (isWindows) {
    files.push(
      "codex-resources/codex-command-runner.exe",
      "codex-resources/codex-windows-sandbox-setup.exe",
    );
  } else if (platform.os[0] === "linux") {
    files.push("codex-resources/bwrap");
  }
  return files;
}

// Unpack a canonical Codex package archive (built by
// scripts/build_codex_package.py in the release workflows) into the platform
// package root. tar preserves the executable bits recorded by the builder,
// which the daemon requires for every packaged helper.
function stagePackage(platform, packageDir, artifactsDir) {
  const artifactName = `codex-package-${platform.target}`;
  const archivePath = join(artifactsDir, artifactName, `${artifactName}.tar.gz`);
  if (!existsSync(archivePath)) {
    throw new Error(`Missing package archive: ${archivePath}`);
  }
  run("tar", ["-xzf", archivePath, "-C", packageDir]);

  const manifestPath = join(packageDir, "codex-package.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const expectedEntrypoint =
    platform.os[0] === "win32" ? "bin/codex.exe" : "bin/codex";
  if (
    manifest.target !== platform.target ||
    manifest.entrypoint !== expectedEntrypoint
  ) {
    throw new Error(
      `Package manifest mismatch in ${archivePath}: ` +
        `target=${manifest.target} entrypoint=${manifest.entrypoint}`,
    );
  }

  for (const name of requiredPackageFiles(platform)) {
    const path = join(packageDir, name);
    if (!existsSync(path)) {
      throw new Error(`Package ${platform.packageName} is missing ${name}`);
    }
    if (platform.os[0] !== "win32" && name !== "codex-package.json") {
      if ((statSync(path).mode & 0o111) === 0) {
        throw new Error(
          `Package ${platform.packageName} file is not executable: ${name}`,
        );
      }
    }
  }
}

function replaceTemplate(templatePath, replacements) {
  let content = readFileSync(templatePath, "utf8");
  for (const [token, replacement] of Object.entries(replacements)) {
    content = content.split(token).join(replacement);
  }
  return content;
}

function writeJson(path, content) {
  const parsed = JSON.parse(content);
  writeFileSync(path, `${JSON.stringify(parsed, null, 2)}\n`);
}

function repositoryMetadata(repository) {
  return {
    homepage: `https://github.com/${repository}`,
    repositoryJson: JSON.stringify({
      type: "git",
      url: `git+https://github.com/${repository}.git`,
    }),
  };
}

function validatePackage(packageDir) {
  const result = run(
    "npm",
    ["pack", "--dry-run", "--ignore-scripts", "--json"],
    { cwd: packageDir, capture: true },
  );
  const packed = JSON.parse(result.stdout)[0];
  console.log(
    `  ${packed.name}: ${packed.files.length} files, ${packed.size} bytes packed`,
  );
}

function assemble(args) {
  validateScope(args.scope);
  validateRepository(args.repo);
  const version = validateVersion(args.version);
  const { homepage, repositoryJson } = repositoryMetadata(args.repo);
  const mainPackage = `${args.scope}/codex`;

  const artifactsDir = resolve(args.artifactsDir);
  const outDir = resolve(args.outDir);
  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });

  const optionalDependencies = {};
  for (const platform of platforms) {
    const packageName = `${args.scope}/${platform.packageName}`;
    optionalDependencies[packageName] = version;

    const packageDir = join(outDir, platform.packageName);
    mkdirSync(packageDir, { recursive: true });
    stagePackage(platform, packageDir, artifactsDir);

    const platformJson = replaceTemplate(
      join(templatesDir, "platform-package.json.tmpl"),
      {
        __PACKAGE__: packageName,
        __VERSION__: version,
        __PLATFORM__: `${platform.os[0]}-${platform.cpu[0]}`,
        __OS__: JSON.stringify(platform.os),
        __CPU__: JSON.stringify(platform.cpu),
        __HOMEPAGE__: homepage,
        __REPOSITORY__: repositoryJson,
      },
    );
    writeJson(join(packageDir, "package.json"), platformJson);
    writeFileSync(
      join(packageDir, "README.md"),
      replaceTemplate(join(templatesDir, "platform-README.md"), {
        __PACKAGE__: packageName,
        __VERSION__: version,
        __PLATFORM__: `${platform.os[0]}-${platform.cpu[0]}`,
        __REPO__: args.repo,
      }),
    );
    copyFileSync(join(repoRoot, "LICENSE"), join(packageDir, "LICENSE"));
  }

  const mainDir = join(outDir, "codex");
  mkdirSync(join(mainDir, "bin"), { recursive: true });
  const platformPackages = Object.fromEntries(
    platforms.map((platform) => [
      `${platform.os[0]}-${platform.cpu[0]}`,
      `${args.scope}/${platform.packageName}`,
    ]),
  );
  writeFileSync(
    join(mainDir, "bin", "codex.js"),
    replaceTemplate(join(templatesDir, "bin-codex.js"), {
      __MAIN_PACKAGE__: mainPackage,
      __PLATFORM_PACKAGES__: JSON.stringify(platformPackages, null, 2),
    }),
  );
  chmodSync(join(mainDir, "bin", "codex.js"), 0o755);

  writeJson(
    join(mainDir, "package.json"),
    replaceTemplate(join(templatesDir, "main-package.json.tmpl"), {
      __MAIN_PACKAGE__: mainPackage,
      __VERSION__: version,
      __OPTIONAL_DEPENDENCIES__: JSON.stringify(optionalDependencies, null, 2),
      __HOMEPAGE__: homepage,
      __REPOSITORY__: repositoryJson,
    }),
  );
  writeFileSync(
    join(mainDir, "README.md"),
    replaceTemplate(join(templatesDir, "main-README.md"), {
      __PACKAGE__: mainPackage,
      __VERSION__: version,
      __REPO__: args.repo,
    }),
  );
  copyFileSync(join(repoRoot, "LICENSE"), join(mainDir, "LICENSE"));

  for (const platform of platforms) {
    console.log(`Validating ${args.scope}/${platform.packageName}@${version}`);
    validatePackage(join(outDir, platform.packageName));
  }
  console.log(`Validating ${mainPackage}@${version}`);
  validatePackage(mainDir);
  console.log(`\nAssembled ${mainPackage}@${version} in ${outDir}`);
}

export function assemblePackages(options) {
  assemble({
    artifactsDir: options.artifactsDir ?? join(repoRoot, "release-assets"),
    outDir: options.outDir ?? join(distRoot, "publish"),
    version: options.version,
    scope: options.scope ?? "@kingingwang",
    repo: options.repo ?? "KingingWang/codex",
  });
}

function main() {
  try {
    const args = parseArgs(process.argv.slice(2));
    args.version ??= defaultVersion(cargoVersion());
    assemble(args);
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  }
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  main();
}

export { parseArgs, usage };

#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const defaultPublishDir = resolve(scriptDir, "../publish");

export const packageOrder = [
  "codex-linux-x64",
  "codex-linux-arm64",
  "codex-darwin-x64",
  "codex-darwin-arm64",
  "codex-win32-x64",
  "codex-win32-arm64",
  "codex",
];

function usage() {
  return `Usage: node npm-dist/scripts/publish.mjs [options]

Options:
  --dry-run               Run npm publish in dry-run mode
  --dist-tag <tag>        npm distribution tag (default: latest)
  --publish-dir <dir>     Assembled package directory (default: npm-dist/publish)
  --help                  Show this help

Platform packages are published before the main package so its optional
dependencies are already resolvable. Published versions are skipped.`;
}

function parseArgs(argv) {
  const args = {
    dryRun: false,
    distTag: "latest",
    publishDir: defaultPublishDir,
  };

  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    if (arg === "--dry-run") {
      args.dryRun = true;
    } else if (arg === "--dist-tag") {
      const value = argv[++index];
      if (!value) throw new Error("--dist-tag requires a value");
      args.distTag = value;
    } else if (arg === "--publish-dir") {
      const value = argv[++index];
      if (!value) throw new Error("--publish-dir requires a value");
      args.publishDir = value;
    } else if (arg === "--help") {
      console.log(usage());
      process.exit(0);
    } else {
      throw new Error(`Unknown option: ${arg}`);
    }
  }
  return args;
}

function validateDistTag(tag) {
  if (!/^[a-z][a-z0-9-._]*$/i.test(tag)) {
    throw new Error(`Invalid npm distribution tag: ${tag}`);
  }
  return tag;
}

function npm() {
  return process.platform === "win32" ? "npm.cmd" : "npm";
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd,
    encoding: "utf8",
    stdio: options.capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    if (options.capture) {
      const output = [result.stdout, result.stderr].filter(Boolean).join("\n");
      throw new Error(output);
    }
    process.exit(result.status ?? 1);
  }
  return result;
}

export function publishArgs({ dryRun, distTag }) {
  return [
    "publish",
    "--access",
    "public",
    "--ignore-scripts",
    "--tag",
    distTag,
    ...(dryRun ? ["--dry-run"] : []),
  ];
}

function packageMetadata(publishDir, packageName) {
  const packageDir = join(publishDir, packageName);
  const packageJsonPath = join(packageDir, "package.json");
  if (!existsSync(packageJsonPath)) {
    throw new Error(
      `Missing assembled package: ${packageJsonPath}. Run assemble.mjs first.`,
    );
  }
  return {
    directory: packageDir,
    ...JSON.parse(readFileSync(packageJsonPath, "utf8")),
  };
}

function isPublished(name, version) {
  const result = spawnSync(npm(), ["view", `${name}@${version}`, "version"], {
    encoding: "utf8",
  });
  if (result.status === 0) return true;

  const output = `${result.stderr ?? ""}\n${result.stdout ?? ""}`;
  if (/\bE404\b|404 Not Found/i.test(output)) return false;

  const detail = output.trim().split("\n")[0];
  throw new Error(
    detail
      ? `Failed to check whether ${name}@${version} is published: ${detail}`
      : `Failed to check whether ${name}@${version} is published`,
  );
}

function publish(args) {
  validateDistTag(args.distTag);
  const publishDir = resolve(args.publishDir);
  const packages = packageOrder.map((name) =>
    packageMetadata(publishDir, name),
  );
  const versions = new Set(packages.map((packageJson) => packageJson.version));
  if (versions.size !== 1) {
    throw new Error(
      `Assembled packages are not lockstep versioned: ${[...versions].join(", ")}`,
    );
  }

  console.log(
    `Publishing ${packages.at(-1).name}@${packages.at(-1).version}${args.dryRun ? " (dry run)" : ""}\n`,
  );
  for (const packageJson of packages) {
    if (!args.dryRun && isPublished(packageJson.name, packageJson.version)) {
      console.log(
        `Skipping ${packageJson.name}@${packageJson.version}: already published`,
      );
      continue;
    }

    console.log(`Publishing ${packageJson.name}@${packageJson.version}...`);
    run(npm(), publishArgs(args), { cwd: packageJson.directory });
  }
  console.log(args.dryRun ? "Dry run complete." : "Publish complete.");
}

function main() {
  try {
    publish(parseArgs(process.argv.slice(2)));
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  }
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  main();
}

export { parseArgs, usage, validateDistTag };

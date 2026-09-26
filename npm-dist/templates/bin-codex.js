#!/usr/bin/env node
// Fork distribution entry point for the Codex CLI.

import { spawn } from "node:child_process";
import { existsSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const MAIN_PACKAGE = "__MAIN_PACKAGE__";
const PLATFORM_PACKAGES = __PLATFORM_PACKAGES__;

const entrypoint = fileURLToPath(import.meta.url);
const entrypointDir = path.dirname(entrypoint);
const codexPackageRoot = realpathSync(path.join(entrypointDir, ".."));
const requireFromCodex = createRequire(import.meta.url);

const { platform, arch } = process;
const platformKey = `${platform}-${arch}`;
const platformPackage = PLATFORM_PACKAGES[platformKey];

if (!platformPackage) {
  throw new Error(`Unsupported platform: ${platformKey}`);
}

function findCodexExecutable() {
  let packageJsonPath;
  try {
    packageJsonPath = requireFromCodex.resolve(
      `${platformPackage}/package.json`,
    );
  } catch {
    throw new Error(
      `Missing optional dependency ${platformPackage}. Reinstall Codex: npm install -g ${MAIN_PACKAGE}@latest`,
    );
  }

  const binaryName = platform === "win32" ? "codex.exe" : "codex";
  const binaryPath = path.join(
    path.dirname(packageJsonPath),
    "bin",
    binaryName,
  );
  if (!existsSync(binaryPath)) {
    throw new Error(
      `Codex binary is missing from ${platformPackage}: ${binaryPath}`,
    );
  }
  return binaryPath;
}

function isPnpmOwnedCodexInstall(nodeModulesDir) {
  if (!existsSync(path.join(nodeModulesDir, ".modules.yaml"))) {
    return false;
  }

  try {
    return (
      realpathSync(path.join(nodeModulesDir, ...MAIN_PACKAGE.split("/"))) ===
      codexPackageRoot
    );
  } catch {
    return false;
  }
}

function detectPackageManager() {
  for (
    let currentDir = codexPackageRoot;
    currentDir !== path.parse(currentDir).root;
    currentDir = path.dirname(currentDir)
  ) {
    if (isPnpmOwnedCodexInstall(path.join(currentDir, "node_modules"))) {
      return "pnpm";
    }
  }

  const userAgent = process.env.npm_config_user_agent || "";
  if (/\bbun\//.test(userAgent)) return "bun";
  if (/\bpnpm\//.test(userAgent)) return "pnpm";

  const execPath = process.env.npm_execpath || "";
  if (execPath.includes("bun")) return "bun";
  if (execPath.includes("pnpm")) return "pnpm";

  if (
    entrypointDir.includes(".bun/install/global") ||
    entrypointDir.includes(".bun\\install\\global")
  ) {
    return "bun";
  }

  return userAgent ? "npm" : null;
}

const packageManager = detectPackageManager();
const packageManagerEnvVar =
  packageManager === "bun"
    ? "CODEX_MANAGED_BY_BUN"
    : packageManager === "pnpm"
      ? "CODEX_MANAGED_BY_PNPM"
      : "CODEX_MANAGED_BY_NPM";
const env = {
  ...process.env,
  CODEX_MANAGED_PACKAGE_ROOT: codexPackageRoot,
};
delete env.CODEX_MANAGED_BY_NPM;
delete env.CODEX_MANAGED_BY_BUN;
delete env.CODEX_MANAGED_BY_PNPM;
env[packageManagerEnvVar] = "1";

const child = spawn(findCodexExecutable(), process.argv.slice(2), {
  stdio: "inherit",
  env,
  windowsHide: true,
});

child.on("error", (error) => {
  console.error(error);
  process.exit(1);
});

function forwardSignal(signal) {
  if (child.killed) return;
  try {
    child.kill(signal);
  } catch {
    // The child may have exited between callback registration and this signal.
  }
}

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => forwardSignal(signal));
}

const result = await new Promise((resolve) => {
  child.on("exit", (code, signal) => {
    if (signal) {
      resolve({ signal });
    } else {
      resolve({ code: code ?? 1 });
    }
  });
});

if (result.signal) {
  process.kill(process.pid, result.signal);
} else {
  process.exit(result.code);
}

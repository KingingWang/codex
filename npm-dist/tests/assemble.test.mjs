import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  assemblePackages,
  cargoVersion,
  defaultVersion,
  platforms,
  validateRepository,
  validateScope,
  validateVersion,
} from "../scripts/assemble.mjs";
import { packageOrder, publishArgs } from "../scripts/publish.mjs";

const binaryHeaders = {
  "elf": [0x7f, 0x45, 0x4c, 0x46],
  "mach-o": [0xcf, 0xfa, 0xed, 0xfe],
  "pe": [0x4d, 0x5a],
};

test("validates metadata inputs", () => {
  assert.equal(validateScope("@kingingwang"), "@kingingwang");
  assert.equal(validateRepository("KingingWang/codex"), "KingingWang/codex");
  assert.equal(
    validateVersion("0.147.0-fork.20260812000000"),
    "0.147.0-fork.20260812000000",
  );
  assert.throws(() => validateScope("kingingwang"));
  assert.throws(() => validateRepository("KingingWang"));
  assert.throws(() => validateVersion("0.147"));
});

test("derives a reproducible fork version", () => {
  assert.equal(
    defaultVersion("0.147.0", "1723789880"),
    "0.147.0-fork.20240816063120",
  );
});

test("platforms and publish order put every binary package before the main package", () => {
  assert.deepEqual(
    platforms.map((platform) => platform.packageName),
    packageOrder.slice(0, -1),
  );
  assert.equal(packageOrder.at(-1), "codex");
});

test("builds valid package metadata from release artifacts", (t) => {
  const root = mkdtempSync(join(tmpdir(), "codex-npm-dist-test-"));
  t.after(() => {
    rmSync(root, { recursive: true, force: true });
  });

  const artifactsDir = join(root, "artifacts");
  const outDir = join(root, "publish");
  for (const platform of platforms) {
    const artifactDir = join(artifactsDir, platform.artifactName);
    mkdirSync(artifactDir, { recursive: true });
    const artifactPath = join(artifactDir, platform.artifactFile);
    const binaryPath = join(root, `${platform.artifactName}.binary`);
    writeFileSync(
      binaryPath,
      Buffer.from([...binaryHeaders[platform.format], 0, 0]),
    );
    chmodSync(binaryPath, 0o755);

    if (platform.compressed) {
      const result = spawnSync("zstd", [
        "-q",
        "-f",
        binaryPath,
        "-o",
        artifactPath,
      ]);
      assert.equal(result.status, 0, result.stderr?.toString());
    } else {
      writeFileSync(artifactPath, readFileSync(binaryPath));
    }
  }

  const version = "0.147.0-fork.20260812000000";
  assemblePackages({
    artifactsDir,
    outDir,
    version,
    scope: "@test",
    repo: "example/codex",
  });

  for (const platform of platforms) {
    const packageJson = JSON.parse(
      readFileSync(join(outDir, platform.packageName, "package.json"), "utf8"),
    );
    assert.deepEqual(packageJson, {
      name: `@test/${platform.packageName}`,
      version,
      description: `Codex CLI binary for ${platform.os[0]}-${platform.cpu[0]} (fork distribution)`,
      license: "Apache-2.0",
      os: platform.os,
      cpu: platform.cpu,
      files: ["bin", "README.md", "LICENSE"],
      publishConfig: { access: "public" },
      homepage: "https://github.com/example/codex",
      repository: {
        type: "git",
        url: "git+https://github.com/example/codex.git",
      },
    });
  }

  const optionalDependencies = Object.fromEntries(
    platforms.map((platform) => [`@test/${platform.packageName}`, version]),
  );
  const mainPackage = JSON.parse(
    readFileSync(join(outDir, "codex", "package.json"), "utf8"),
  );
  assert.deepEqual(mainPackage, {
    name: "@test/codex",
    version,
    description: "Codex CLI fork distribution with platform-specific binaries",
    license: "Apache-2.0",
    type: "module",
    bin: { codex: "bin/codex.js" },
    files: ["bin", "README.md", "LICENSE"],
    engines: { node: ">=18" },
    optionalDependencies,
    publishConfig: { access: "public" },
    homepage: "https://github.com/example/codex",
    repository: {
      type: "git",
      url: "git+https://github.com/example/codex.git",
    },
  });

  const wrapper = readFileSync(
    join(outDir, "codex", "bin", "codex.js"),
    "utf8",
  );
  assert.match(wrapper, /const MAIN_PACKAGE = "@test\/codex";/);
  assert.doesNotMatch(wrapper, /@openai\/codex/);

  const mainReadme = readFileSync(join(outDir, "codex", "README.md"), "utf8");
  assert.match(mainReadme, /# @test\/codex/);
  assert.match(mainReadme, /\[example\/codex\]/);
  assert.doesNotMatch(mainReadme, /\*\*(?:PACKAGE|REPO)\*\*/);
});

test("uses public npm publish arguments and a selected distribution tag", () => {
  assert.deepEqual(publishArgs({ dryRun: false, distTag: "latest" }), [
    "publish",
    "--access",
    "public",
    "--ignore-scripts",
    "--tag",
    "latest",
  ]);
  assert.deepEqual(publishArgs({ dryRun: true, distTag: "next" }), [
    "publish",
    "--access",
    "public",
    "--ignore-scripts",
    "--tag",
    "next",
    "--dry-run",
  ]);
});

test("reads the Codex workspace version", () => {
  assert.equal(validateVersion(cargoVersion()), cargoVersion());
});

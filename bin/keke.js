#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");

const PLATFORM_PACKAGE_BY_KEY = {
  "darwin-x64": "@milisp/keke-darwin-x64",
  "darwin-arm64": "@milisp/keke-darwin-arm64",
  "linux-x64": "@milisp/keke-linux-x64",
  "linux-arm64": "@milisp/keke-linux-arm64",
  "win32-x64": "@milisp/keke-win32-x64",
};

const { platform, arch } = process;
const pkgName = PLATFORM_PACKAGE_BY_KEY[`${platform}-${arch}`];

if (!pkgName) {
  console.error(`[keke] unsupported platform: ${platform}-${arch}`);
  process.exit(1);
}

const binName = platform === "win32" ? "keke.exe" : "keke";

let bin;
try {
  const pkgJsonPath = require.resolve(`${pkgName}/package.json`);
  bin = pkgJsonPath.replace(/package\.json$/, binName);
} catch {
  console.error(
    `[keke] optional dependency ${pkgName} is missing. This usually ` +
      "means npm skipped it (e.g. --no-optional, or an outdated lockfile). " +
      "Try:\n" +
      "  npm install @milisp/keke@latest --force"
  );
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`[keke] failed to run binary: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);

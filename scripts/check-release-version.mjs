#!/usr/bin/env node

import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const semverPattern = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

const repoRoot = process.env.PRMONITOR_RELEASE_ROOT
  ? process.env.PRMONITOR_RELEASE_ROOT
  : join(dirname(fileURLToPath(import.meta.url)), "..");

function requireRepoRoot() {
  const markers = ["package.json", join("src-tauri", "Cargo.toml"), join("src-tauri", "tauri.conf.json")];
  for (const marker of markers) {
    if (!existsSync(join(repoRoot, marker))) {
      throw new Error(
        `Cannot find repository root marker '${marker}' next to scripts/ (resolved root: ${repoRoot}). Run from the prmonitor checkout or invoke via 'node scripts/check-release-version.mjs'.`,
      );
    }
  }
}

function readJson(relativePath) {
  const path = join(repoRoot, relativePath);
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    throw new Error(`Cannot read or parse ${relativePath}: ${error.message}`);
  }
}

function readCargoPackageVersion(relativePath) {
  const path = join(repoRoot, relativePath);
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    throw new Error(`Cannot read ${relativePath}: ${error.message}`);
  }

  let inPackageSection = false;
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (/^\[[^\]]+\]$/.test(trimmed)) {
      if (trimmed === "[package]") {
        inPackageSection = true;
        continue;
      }
      if (inPackageSection) {
        break;
      }
    }

    if (inPackageSection) {
      const match = line.match(/^\s*version\s*=\s*["']([^"']+)["']/);
      if (match) {
        return match[1];
      }
    }
  }

  throw new Error(`Cannot find [package].version in ${relativePath}`);
}

function assertSemver(label, value) {
  if (typeof value !== "string" || !semverPattern.test(value)) {
    throw new Error(`${label} has invalid version '${String(value)}'`);
  }
}

try {
  requireRepoRoot();

  const versions = {
    "package.json": readJson("package.json").version,
    "src-tauri/Cargo.toml": readCargoPackageVersion("src-tauri/Cargo.toml"),
    "src-tauri/tauri.conf.json": readJson("src-tauri/tauri.conf.json").version,
  };

  for (const [file, version] of Object.entries(versions)) {
    assertSemver(file, version);
  }

  const requested = process.argv[2]?.replace(/^v/, "");
  if (requested !== undefined) {
    assertSemver("requested release", requested);
  }

  const expected = requested ?? versions["package.json"];
  const mismatches = Object.entries(versions).filter(([, version]) => version !== expected);

  console.log(`Expected release version: ${expected}`);
  for (const [file, version] of Object.entries(versions)) {
    console.log(`- ${file}: ${version}`);
  }

  if (mismatches.length > 0) {
    throw new Error(
      `Version mismatch: ${mismatches.map(([file, version]) => `${file}=${version}`).join(", ")}`,
    );
  }

  console.log("Release versions are consistent.");
} catch (error) {
  console.error(`Release version check failed: ${error.message}`);
  process.exit(1);
}

import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

const scriptPath = fileURLToPath(new URL("./check-release-version.mjs", import.meta.url));

function writeProject(root, versions) {
  writeFileSync(join(root, "package.json"), JSON.stringify({ version: versions.pkg }, null, 2));
  mkdirSync(join(root, "src-tauri"), { recursive: true });
  writeFileSync(
    join(root, "src-tauri", "Cargo.toml"),
    `[package]\nname = "prmonitor"\nversion = "${versions.cargo}"\nedition = "2021"\n\n[dependencies]\nserde = "1"\n`,
  );
  writeFileSync(
    join(root, "src-tauri", "tauri.conf.json"),
    JSON.stringify({ version: versions.tauri }, null, 2),
  );
}

function runCheck(root, args = []) {
  return spawnSync(process.execPath, [scriptPath, ...args], {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      PRMONITOR_RELEASE_ROOT: root,
    },
  });
}

test("consistent versions pass without expected arg", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    writeProject(root, { pkg: "0.1.1", cargo: "0.1.1", tauri: "0.1.1" });
    const result = runCheck(root);
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /Release versions are consistent/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("consistent versions pass with v-prefixed expected", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    writeProject(root, { pkg: "0.1.1", cargo: "0.1.1", tauri: "0.1.1" });
    const result = runCheck(root, ["v0.1.1"]);
    assert.equal(result.status, 0, result.stderr);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("prerelease versions pass when consistent", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    writeProject(root, { pkg: "0.1.2-beta.1", cargo: "0.1.2-beta.1", tauri: "0.1.2-beta.1" });
    const result = runCheck(root, ["v0.1.2-beta.1"]);
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /0\.1\.2-beta\.1/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("mismatched project versions fail", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    writeProject(root, { pkg: "0.1.1", cargo: "0.1.0", tauri: "0.1.1" });
    const result = runCheck(root);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /Version mismatch/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("expected version mismatch fails", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    writeProject(root, { pkg: "0.1.1", cargo: "0.1.1", tauri: "0.1.1" });
    const result = runCheck(root, ["9.9.9"]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /Version mismatch/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("invalid expected semver fails", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    writeProject(root, { pkg: "0.1.1", cargo: "0.1.1", tauri: "0.1.1" });
    const result = runCheck(root, ["not-a-version"]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /invalid version/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("leading-zero versions rejected by semver single source", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    writeProject(root, { pkg: "01.0.0", cargo: "01.0.0", tauri: "01.0.0" });
    const result = runCheck(root, ["v01.0.0"]);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /invalid version/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("missing required files fail with Cannot read", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    const result = runCheck(root);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /Cannot find repository root marker|Cannot read/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("dependency version lines outside [package] are ignored", () => {
  const root = mkdtempSync(join(tmpdir(), "release-version-"));
  try {
    writeFileSync(join(root, "package.json"), JSON.stringify({ version: "0.1.1" }, null, 2));
    mkdirSync(join(root, "src-tauri"), { recursive: true });
    writeFileSync(
      join(root, "src-tauri", "Cargo.toml"),
      `[package]\nname = "prmonitor"\nversion = "0.1.1"\nedition = "2021"\n\n[dependencies]\nserde = { version = "1.0.0" }\n`,
    );
    writeFileSync(
      join(root, "src-tauri", "tauri.conf.json"),
      JSON.stringify({ version: "0.1.1" }, null, 2),
    );
    const result = runCheck(root, ["0.1.1"]);
    assert.equal(result.status, 0, result.stderr);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("default root resolves via scripts/ parent when env unset", () => {
  const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
  const cleanEnv = Object.fromEntries(
    Object.entries(process.env).filter(([key]) => key !== "PRMONITOR_RELEASE_ROOT"),
  );
  const result = spawnSync(process.execPath, [scriptPath, "0.1.1"], {
    cwd: join(repoRoot, "src"),
    encoding: "utf8",
    env: cleanEnv,
  });
  assert.equal(result.status, 0, result.stderr);
});

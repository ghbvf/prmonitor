// Slice-boundary enforcement test (Medium, per .claude/rules/prmonitor/ai-robust.md).
//
// The vertical slices (config / pr / review / inbox / outbox / terminal / messaging / workflow) must be
// self-contained: a file in one slice must NOT have a runtime (value) import that resolves into a SIBLING
// slice's directory. Cross-slice wiring is the composition root's job (App.vue, which
// lives at the `src/` root, not inside any slice). Before this test, the only carrier
// for that rule was a hand-maintained convention (Soft) — eyeballing imports during
// review. This test makes the violation machine-checkable in CI: a re-introduced
// value import like `import { useConfigStore } from "../config/useConfigStore"` inside
// `src/pr/` fails the suite.
//
// Carrier rationale (why Medium, not Hard): a true Hard carrier would make the
// violation un-expressible (e.g. separate TS project references / build graph per
// slice). That's a larger build-config change; this lexical scan is the low-cost
// Medium that catches the expressible violation in CI. A Hard path is left as a
// future option (登记 GitHub Issue if pursued).
//
// Source access uses Vite's `import.meta.glob(..., '?raw')` rather than node `fs` —
// this keeps the test inside the existing dep/type surface (no `@types/node`, no new
// deps) and resolves files the same way Vite does.
//
// What is ALLOWED (not a violation):
//   - Imports to `../types`, `../api`, or any other `src/` ROOT module — the root is
//     the shared contract surface, not a slice.
//   - TYPE-ONLY cross-slice imports (`import type { AppConfig } from "../config/types"`):
//     these are erased at compile time and create NO runtime dependency edge between
//     slices. WebhookPanel (pr) needs AppConfig's shape for its props; duplicating the
//     shape would be a worse drift hazard than a type-only reference. The runtime
//     coupling this test guards against (e.g. a pr file instantiating config's Pinia
//     store) is exactly what `useConfigStore` would reintroduce — and that IS caught.
//   - The composition root `App.vue` (at `src/`, not in a slice) may cross slices.
import { describe, expect, it } from "vitest";

const SLICES = ["config", "pr", "review", "inbox", "outbox", "terminal", "messaging", "workflow"] as const;
type Slice = (typeof SLICES)[number];

// Eager-load every slice source file as raw text. Keys are paths relative to THIS
// file, e.g. "./pr/WebhookPanel.vue". Vue SFCs are plain text here — import
// statements only ever appear inside <script>, so scanning the whole file is safe.
const sources = import.meta.glob("./{config,pr,review,inbox,outbox,terminal,messaging,workflow}/**/*.{ts,vue}", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

// The Transport port (AB#1375): `@tauri-apps/*` desktop APIs (invoke/listen/opener)
// must be reachable from EXACTLY ONE module — the Tauri transport adapter. Every other
// file talks to the backend through the runtime-selected `Transport`, so the browser
// SPA build can swap in HttpTransport and never bundle (or crash on) a desktop-only API.
// The single legal importer:
const TAURI_TRANSPORT_MODULE = "./transport/tauri.ts";

// Root modules are either composition modules (allowed to wire slices together) or
// shared modules (imported by slices as contracts/helpers). A shared root module must
// not hide a runtime dependency back into a slice: that would let a slice import the
// root carrier and transitively depend on a sibling, bypassing the direct-edge guard.
// Keep the small composition set explicit; every other production file directly under
// `src/` is governed by the shared-root rule below.
const ROOT_COMPOSITION_MODULES = new Set([
  "./App.vue",
  "./RemoteTerminalApp.vue",
  "./RemoteWebConsoleApp.vue",
  "./StatusBar.vue",
  "./main.ts",
  "./projects.ts",
]);

// Eager-load EVERY src file as raw text (not just slices) so the tauri-import funnel is
// CLOSED on the upstream side — a stray `@tauri-apps/*` import (static OR dynamic, see
// extractImports) ANYWHERE under src/ is caught, not only inside a slice. `*.test.ts`
// files are exempt for two independent reasons: (1) a test's `vi.mock("@tauri-apps/...")`
// is a call argument, not an import statement, so extractImports never matches it anyway;
// (2) test files are never included in the production SPA bundle, so even a direct
// `@tauri-apps` import in a test does not violate browser safety.
const allSources = import.meta.glob("./**/*.{ts,vue}", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

// Which slice owns a glob key like "./pr/WebhookPanel.vue".
function ownerSlice(key: string): Slice {
  for (const s of SLICES) {
    if (key.startsWith(`./${s}/`)) return s;
  }
  throw new Error(`glob key not under a known slice: ${key}`);
}

interface ImportRef {
  spec: string; // raw module specifier, e.g. "../config/useConfigStore"
  typeOnly: boolean; // `import type ...` / `export type ... from`
}

// Extract every import/re-export specifier from a source file — static AND dynamic —
// flagging the type-only ones. Dynamic `import()` is covered so the tauri-import funnel
// stays closed against `(await import("@tauri-apps/…"))` (the very form main.ts uses to
// load the adapters); without it the upstream scan would have a dynamic-import blind spot.
function extractImports(source: string): ImportRef[] {
  const refs: ImportRef[] = [];
  // `import ... from "x"` and `import type ... from "x"` (default/named/namespace).
  const importRe = /\bimport\s+(type\s+)?[^;'"]*?\bfrom\s*["']([^"']+)["']/g;
  // Bare side-effect import: `import "x"` (no `from`). Never type-only.
  const sideEffectRe = /\bimport\s+["']([^"']+)["']/g;
  // Re-export: `export ... from "x"` / `export type ... from "x"`.
  const reExportRe = /\bexport\s+(type\s+)?[^;'"]*?\bfrom\s*["']([^"']+)["']/g;
  // Dynamic `import("x")` / `import( "x" )` — always a runtime (value) edge, never type-only.
  const dynamicRe = /\bimport\s*\(\s*["']([^"']+)["']\s*\)/g;

  let m: RegExpExecArray | null;
  while ((m = importRe.exec(source)) !== null) {
    refs.push({ spec: m[2], typeOnly: Boolean(m[1]) });
  }
  while ((m = sideEffectRe.exec(source)) !== null) {
    refs.push({ spec: m[1], typeOnly: false });
  }
  while ((m = reExportRe.exec(source)) !== null) {
    refs.push({ spec: m[2], typeOnly: Boolean(m[1]) });
  }
  while ((m = dynamicRe.exec(source)) !== null) {
    refs.push({ spec: m[1], typeOnly: false });
  }
  return refs;
}

// Resolve a relative specifier (from a file in `fromSlice`) to the SIBLING slice it
// points into, or null if it stays in-slice / goes to the shared `src/` root / is an
// external package. We only need to detect the cross-slice case, so we resolve the
// leading `../` hops against the file's slice directory: a spec starting `../<slice>/`
// from inside a different slice is the cross-slice case.
function crossSliceTarget(fromSlice: Slice, spec: string): Slice | null {
  if (!spec.startsWith(".")) return null; // external package
  // Drop a single leading "./" or any number of "../" to expose the target head.
  const m = spec.match(/^(?:\.\/|(?:\.\.\/)+)(.*)$/);
  if (!m) return null;
  const upCount = (spec.match(/\.\.\//g) ?? []).length;
  // From `src/<slice>/...`, exactly one `../` reaches `src/`. Going up to src/ and
  // then into a sibling slice is the only cross-slice relative form (deeper nesting
  // would need a matching number of hops; this app keeps slice files one level deep).
  if (upCount < 1) return null; // "./x" stays in the same slice
  const head = m[1].split("/")[0]; // first path segment after the hops
  for (const s of SLICES) {
    if (s !== fromSlice && head === s) return s;
  }
  return null; // resolves to src/ root (shared) or same slice — allowed
}

// Resolve a relative import from a root module to a slice. Unlike
// `crossSliceTarget`, root files reach a slice with `./<slice>/...`.
function rootSliceTarget(spec: string): Slice | null {
  const match = spec.match(/^\.\/([^/]+)(?:\/|$)/);
  if (!match) return null;
  return SLICES.find((slice) => slice === match[1]) ?? null;
}

function isSharedRootModule(key: string): boolean {
  if (!/^\.\/[^/]+\.(?:ts|vue)$/.test(key)) return false;
  if (key.endsWith(".test.ts") || key.endsWith(".d.ts")) return false;
  return !ROOT_COMPOSITION_MODULES.has(key);
}

describe("vertical slice boundary (config / pr / review / inbox / outbox / terminal)", () => {
  it("loaded slice sources for all declared slices", () => {
    // Guard against the glob silently matching nothing (which would make the next
    // assertion vacuously pass and let a real violation through).
    const seen = new Set(Object.keys(sources).map(ownerSlice));
    expect([...seen].sort()).toEqual([...SLICES].sort());
  });

  it("no shared root module imports or re-exports runtime values from a slice", () => {
    const violations: string[] = [];

    for (const [key, source] of Object.entries(allSources)) {
      if (!isSharedRootModule(key)) continue;
      for (const ref of extractImports(source)) {
        const target = rootSliceTarget(ref.spec);
        if (target === null || ref.typeOnly) continue;
        violations.push(`${key} imports value from slice '${target}': ${ref.spec}`);
      }
    }

    expect(violations).toEqual([]);
  });

  it("no slice has a runtime (value) import into a sibling slice", () => {
    const violations: string[] = [];

    for (const [key, source] of Object.entries(sources)) {
      const slice = ownerSlice(key);
      for (const ref of extractImports(source)) {
        const target = crossSliceTarget(slice, ref.spec);
        if (target === null) continue; // same slice / root / external — allowed
        // Type-only cross-slice imports are erased at compile time → no runtime edge
        // → allowed (see header rationale).
        if (ref.typeOnly) continue;
        violations.push(`${key} imports value from sibling slice '${target}': ${ref.spec}`);
      }
    }

    expect(violations).toEqual([]);
  });

  // Sanity-check the detector itself: a synthetic value cross-slice import MUST be
  // flagged, and a type-only one MUST NOT. Guards against the test silently passing
  // because the regex / resolver stopped matching.
  it("detector flags a value cross-slice import but allows a type-only one", () => {
    const valueImport = extractImports(
      `import { useConfigStore } from "../config/useConfigStore";`,
    );
    expect(valueImport).toHaveLength(1);
    expect(valueImport[0].typeOnly).toBe(false);
    // A pr file importing ../config/... is a config-slice target.
    expect(crossSliceTarget("pr", valueImport[0].spec)).toBe("config");

    const typeImport = extractImports(
      `import type { AppConfig } from "../config/types";`,
    );
    expect(typeImport).toHaveLength(1);
    expect(typeImport[0].typeOnly).toBe(true);
    expect(crossSliceTarget("pr", typeImport[0].spec)).toBe("config");

    // Root-module / same-slice specifiers resolve to no sibling slice.
    expect(crossSliceTarget("pr", "../types")).toBeNull();
    expect(crossSliceTarget("pr", "./api")).toBeNull();
  });

  it("shared-root detector flags runtime slice edges but allows type-only edges", () => {
    const refs = extractImports(
      `export { DEFAULT_CONFIG } from "./config/types.generated";\n` +
        `import type { AppConfig } from "./config/types";`,
    );

    const runtimeRef = refs.find((ref) => !ref.typeOnly);
    const typeRef = refs.find((ref) => ref.typeOnly);

    expect(runtimeRef).toBeDefined();
    expect(rootSliceTarget(runtimeRef!.spec)).toBe("config");
    expect(typeRef).toBeDefined();
    expect(rootSliceTarget(typeRef!.spec)).toBe("config");
    expect(isSharedRootModule("./types.ts")).toBe(true);
    expect(isSharedRootModule("./App.vue")).toBe(false);
  });
});

// Tauri-import funnel (Medium, AB#1375). CLOSED funnel: upstream = every src file is
// scanned (allSources), downstream = exactly one exempt module (the Tauri transport
// adapter). A `@tauri-apps/*` import leaking into any other module would make the
// browser SPA build bundle a desktop-only API — caught here in CI.
//
// Carrier rationale (why Medium, not Hard): the Hard form would be a separate browser
// tsconfig/build graph that physically excludes `@tauri-apps/*` from the web target so
// the import is un-resolvable. That is the future `build:web` hardening path; this
// lexical scan is the low-cost Medium that catches the expressible violation now.
describe("tauri-import funnel (only transport/tauri.ts may import @tauri-apps/*)", () => {
  const isExempt = (key: string) =>
    key === TAURI_TRANSPORT_MODULE || key.endsWith(".test.ts");

  it("loaded the single legal tauri-import module", () => {
    // Guard against the exemption pointing at a path the glob never produced (a rename
    // would otherwise make the next assertion vacuously pass).
    expect(Object.keys(allSources)).toContain(TAURI_TRANSPORT_MODULE);
  });

  it("no module outside transport/tauri.ts imports @tauri-apps/*", () => {
    const violations: string[] = [];
    for (const [key, source] of Object.entries(allSources)) {
      if (isExempt(key)) continue;
      for (const ref of extractImports(source)) {
        if (ref.spec.startsWith("@tauri-apps/")) {
          violations.push(`${key} imports desktop-only API: ${ref.spec}`);
        }
      }
    }
    expect(violations).toEqual([]);
  });

  // Sanity-check the detector: a synthetic `@tauri-apps/*` import MUST be extracted and
  // flagged — for BOTH static and dynamic forms (the dynamic form is what main.ts uses to
  // load adapters, so the funnel must see it). Guards against the scan silently passing
  // because the regex stopped matching.
  it("detector flags an @tauri-apps import — static and dynamic (any subpath)", () => {
    const refs = extractImports(
      `import { openUrl } from "@tauri-apps/plugin-opener";\n` +
        `import { invoke } from "@tauri-apps/api/core";\n` +
        `const e = await import("@tauri-apps/api/event");`,
    );
    const tauri = refs.filter((r) => r.spec.startsWith("@tauri-apps/"));
    expect(tauri.map((r) => r.spec).sort()).toEqual([
      "@tauri-apps/api/core",
      "@tauri-apps/api/event",
      "@tauri-apps/plugin-opener",
    ]);
  });
});

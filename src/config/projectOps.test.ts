// Locks the activeProjectId invariant the add/delete draft ops maintain — the crux of
// the deletable-to-empty + empty-state behavior (#config-page). Mirrors the backend
// validate() contract (config/model.rs): non-empty projects ⇒ activeProjectId names an
// existing project; empty list ⇒ empty activeProjectId.
import { describe, it, expect } from "vitest";
import {
  addProjectToDraft,
  deleteProjectFromDraft,
  makeProject,
} from "./projectOps";
import { NEW_PROJECT_DEFAULTS } from "./defaults";
import type { AppConfig, Project } from "./types";

function project(id: string, name = id): Project {
  return { ...NEW_PROJECT_DEFAULTS, id, name };
}

function draft(projects: Project[], activeProjectId: string): AppConfig {
  return {
    projects,
    activeProjectId,
    webhookEnabled: false,
    webhookPort: 8787,
    webhookSecret: "",
    cloudflaredBin: "cloudflared",
    webhookTunnelMode: "quick",
    webhookTunnelCommand: "",
    webhookPublicUrl: "",
    localApiToken: "",
    outbox: { notificationTtlSecs: 7200 },
    notifications: { channels: [] },
    listeners: [],
    tunnels: [],
    rules: [],
  };
}

describe("deleteProjectFromDraft", () => {
  it("removes a non-active project and leaves activeProjectId untouched", () => {
    const d = draft([project("a"), project("b")], "a");
    expect(deleteProjectFromDraft(d, "b")).toBe(true);
    expect(d.projects.map((p) => p.id)).toEqual(["a"]);
    expect(d.activeProjectId).toBe("a");
  });

  it("falls back to the first remaining project when the active one is deleted", () => {
    const d = draft([project("a"), project("b")], "a");
    deleteProjectFromDraft(d, "a");
    expect(d.projects.map((p) => p.id)).toEqual(["b"]);
    expect(d.activeProjectId).toBe("b");
  });

  it("clears activeProjectId to '' when the last project is deleted", () => {
    const d = draft([project("a")], "a");
    deleteProjectFromDraft(d, "a");
    expect(d.projects).toEqual([]);
    expect(d.activeProjectId).toBe("");
  });

  it("is a no-op for an unknown id", () => {
    const d = draft([project("a")], "a");
    expect(deleteProjectFromDraft(d, "zzz")).toBe(false);
    expect(d.projects.map((p) => p.id)).toEqual(["a"]);
    expect(d.activeProjectId).toBe("a");
  });
});

describe("addProjectToDraft", () => {
  it("makes the first project active when the list was empty", () => {
    const d = draft([], "");
    const p = addProjectToDraft(d);
    expect(d.projects.map((x) => x.id)).toEqual([p.id]);
    expect(d.activeProjectId).toBe(p.id);
  });

  it("leaves the existing active selection when adding to a non-empty list", () => {
    const d = draft([project("a")], "a");
    const p = addProjectToDraft(d);
    expect(d.projects.map((x) => x.id)).toEqual(["a", p.id]);
    expect(d.activeProjectId).toBe("a");
  });

  it("round-trips delete-to-empty then re-add, keeping activeProjectId valid", () => {
    const d = draft([project("a")], "a");
    deleteProjectFromDraft(d, "a");
    expect(d.activeProjectId).toBe("");
    const p = addProjectToDraft(d);
    expect(d.activeProjectId).toBe(p.id);
  });
});

describe("makeProject", () => {
  it("seeds from NEW_PROJECT_DEFAULTS with a fresh id and default name", () => {
    const p = makeProject();
    expect(p.name).toBe("新项目");
    expect(p.id).toBeTruthy();
    expect(p.enabled).toBe(NEW_PROJECT_DEFAULTS.enabled);
    expect(makeProject().id).not.toBe(p.id);
  });
});

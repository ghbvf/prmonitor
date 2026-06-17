import { beforeEach, describe, expect, it, vi } from "vitest";

// projects.ts persists the active selection via config/api's setActiveProject; mock it
// so these tests exercise the ordering/rollback contract without a Tauri backend.
vi.mock("./config/api", () => ({
  setActiveProject: vi.fn(),
}));

import * as configApi from "./config/api";
import { useProjects } from "./projects";

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(configApi.setActiveProject).mockResolvedValue();
  // Module-level singleton refs: reset between tests.
  useProjects().activeProjectId.value = "";
  useProjects().projects.value = [];
});

describe("useProjects.setActive (#35 F6: persist-first)", () => {
  it("persists BEFORE updating the local active id", async () => {
    const { setActive, activeProjectId } = useProjects();
    activeProjectId.value = "old";
    // Capture the local id observed AT persist time — it must still be the pre-switch
    // value (persist-first), proving the local update happens only after success.
    let idAtPersist: string | null = null;
    vi.mocked(configApi.setActiveProject).mockImplementationOnce(async () => {
      idAtPersist = activeProjectId.value;
    });

    await setActive("new");

    expect(configApi.setActiveProject).toHaveBeenCalledWith("new");
    expect(idAtPersist).toBe("old"); // persist saw the OLD id (not yet switched)
    expect(activeProjectId.value).toBe("new"); // switched only after persist resolved
  });

  it("does NOT switch the local active id when persist fails", async () => {
    const { setActive, activeProjectId } = useProjects();
    activeProjectId.value = "old";
    vi.mocked(configApi.setActiveProject).mockRejectedValueOnce({ message: "boom" });

    await expect(setActive("new")).rejects.toBeDefined();

    // A failed persist must leave the UI on the current project (no stale switch).
    expect(activeProjectId.value).toBe("old");
  });
});

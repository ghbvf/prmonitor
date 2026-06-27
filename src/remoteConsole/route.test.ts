import { describe, expect, it } from "vitest";
import { isRemoteWebConsolePath } from "./route";

describe("remote web console browser route", () => {
  it("mounts the remote console only under /ui", () => {
    expect(isRemoteWebConsolePath("/ui")).toBe(true);
    expect(isRemoteWebConsolePath("/ui/")).toBe(true);
    expect(isRemoteWebConsolePath("/ui/sessions")).toBe(true);
    expect(isRemoteWebConsolePath("/")).toBe(false);
    expect(isRemoteWebConsolePath("/terminal")).toBe(false);
    expect(isRemoteWebConsolePath("/settings")).toBe(false);
  });
});

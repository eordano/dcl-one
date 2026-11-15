// @vitest-environment jsdom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const assign = vi.fn();

// The first load of the store on a busy machine outlasts a test's time limit; later loads reuse it.
beforeAll(async () => {
  await import("./signin-store");
}, 60_000);

beforeEach(() => {
  vi.resetModules();
  vi.stubGlobal("location", { pathname: "/shop/items", search: "?sortBy=newest", hash: "#grid", assign });
});
afterEach(() => {
  vi.unstubAllGlobals();
  assign.mockClear();
  delete (window as { __DCL_SHELL__?: unknown }).__DCL_SHELL__;
});

describe("asking the reader to sign in", () => {
  it("sends them to the sign-in page, to come back to this page", async () => {
    const { openSignIn, useSignInOpen } = await import("./signin-store");
    openSignIn();
    expect(assign).toHaveBeenCalledWith("/auth/login?redirectTo=%2Fshop%2Fitems%3FsortBy%3Dnewest%23grid");
    expect(useSignInOpen).toBeTypeOf("function");
  });

  it("returns them where the caller says when it says", async () => {
    const { openSignIn } = await import("./signin-store");
    openSignIn({ redirectTo: "/shop/checkout" });
    expect(assign).toHaveBeenCalledWith("/auth/login?redirectTo=%2Fshop%2Fcheckout");
  });

  it("keeps a reader mid-task on the page, in the sign-in modal", async () => {
    const store = await import("./signin-store");
    store.openSignIn({ inPlace: true, redirectTo: "/create/storage?step=edit" });
    expect(assign).not.toHaveBeenCalled();
    expect(store.takePendingRedirect()).toBe("/create/storage?step=edit");
  });

  it("leaves the Explorer's own sign-in, and the desktop shell's modal, as they were", async () => {
    const store = await import("./signin-store");
    const hostSignIn = vi.fn();
    const uninstall = store.installHostSignIn(hostSignIn);
    store.openSignIn();
    expect(hostSignIn).toHaveBeenCalledOnce();
    uninstall();

    (window as { __DCL_SHELL__?: unknown }).__DCL_SHELL__ = { shell: "tauri" };
    const opened: boolean[] = [];
    store.openSignIn({ redirectTo: "/create" });
    opened.push(store.takePendingRedirect() === "/create");
    expect(opened).toEqual([true]);
    expect(assign).not.toHaveBeenCalled();
  });
});

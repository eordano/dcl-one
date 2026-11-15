// @vitest-environment jsdom
import { act, cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from "vitest";

vi.hoisted(() => {
  Object.assign(window, {
    __vite_plugin_react_preamble_installed__: true,
    $RefreshSig$: () => (type: unknown) => type,
    $RefreshReg$: () => {},
  });
});

import type { TurnstileApi } from "./TurnstileWidget";

type Widget = typeof import("./TurnstileWidget").TurnstileWidget;

const scripts = () => document.head.querySelectorAll('script[src^="https://challenges.cloudflare.com/turnstile/"]');

describe("TurnstileWidget", () => {
  let TurnstileWidget: Widget;
  let onToken: Mock<(token: string) => void>;
  let onExpire: Mock<() => void>;
  let onLoadError: Mock<() => void>;

  function renderWidget() {
    return render(<TurnstileWidget siteKey="site-key" onToken={onToken} onExpire={onExpire} onLoadError={onLoadError} />);
  }

  beforeEach(async () => {
    // A fresh module per test: the script loader is shared per page.
    vi.resetModules();
    ({ TurnstileWidget } = await import("./TurnstileWidget"));
    onToken = vi.fn();
    onExpire = vi.fn();
    onLoadError = vi.fn();
  });

  afterEach(() => {
    cleanup();
    delete window.turnstile;
    for (const script of scripts()) script.remove();
    vi.resetAllMocks();
  });

  describe("when the Turnstile API is on the page", () => {
    let api: { render: Mock<TurnstileApi["render"]>; remove: Mock<TurnstileApi["remove"]> };

    beforeEach(() => {
      api = { render: vi.fn(), remove: vi.fn() };
      window.turnstile = api;
    });

    describe("and the widget renders", () => {
      beforeEach(() => {
        api.render.mockReturnValueOnce("widget-1");
      });

      it("should render the challenge with the site key", async () => {
        renderWidget();
        await waitFor(() => expect(api.render.mock.calls[0]?.[1]).toMatchObject({ sitekey: "site-key", theme: "dark" }));
      });

      it("should hand the solved token to the page", async () => {
        renderWidget();
        await waitFor(() => expect(api.render).toHaveBeenCalled());
        act(() => api.render.mock.calls[0]![1].callback("token-1"));
        expect(onToken).toHaveBeenCalledWith("token-1");
      });

      it("should drop the token when it expires", async () => {
        renderWidget();
        await waitFor(() => expect(api.render).toHaveBeenCalled());
        act(() => api.render.mock.calls[0]![1]["expired-callback"]());
        expect(onExpire).toHaveBeenCalledOnce();
      });

      it("should remove the widget on unmount", async () => {
        const view = renderWidget();
        await waitFor(() => expect(api.render).toHaveBeenCalled());
        view.unmount();
        expect(api.remove).toHaveBeenCalledWith("widget-1");
      });
    });

    describe("and rendering the widget throws", () => {
      beforeEach(() => {
        api.render.mockImplementationOnce(() => {
          throw new Error("invalid site key");
        });
      });

      it("should report a load error", async () => {
        renderWidget();
        await waitFor(() => expect(onLoadError).toHaveBeenCalledOnce());
      });
    });
  });

  describe("when the script has to be loaded", () => {
    describe("and it fails to load", () => {
      beforeEach(() => {
        renderWidget();
        act(() => {
          scripts()[0]!.dispatchEvent(new Event("error"));
        });
      });

      it("should report a load error", async () => {
        await waitFor(() => expect(onLoadError).toHaveBeenCalledOnce());
      });

      it("should remove the failed script", async () => {
        await waitFor(() => expect(scripts()).toHaveLength(0));
      });
    });

    describe("and it loads without defining the API", () => {
      let failed: Element;
      let render: Mock<TurnstileApi["render"]>;

      beforeEach(async () => {
        renderWidget();
        failed = scripts()[0]!;
        act(() => {
          failed.dispatchEvent(new Event("load"));
        });
        await waitFor(() => expect(onLoadError).toHaveBeenCalledOnce());
        cleanup();
        render = vi.fn();
        renderWidget();
      });

      it("should add a new script on the next mount", () => {
        expect(scripts()[0]).not.toBe(failed);
      });

      it("should render the challenge once the new script defines the API", async () => {
        window.turnstile = { render, remove: vi.fn() };
        act(() => {
          scripts()[0]!.dispatchEvent(new Event("load"));
        });
        await waitFor(() => expect(render).toHaveBeenCalledOnce());
      });
    });
  });
});

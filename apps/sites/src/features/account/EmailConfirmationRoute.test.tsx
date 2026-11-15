// @vitest-environment jsdom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.hoisted(() => {
  Object.assign(window, {
    __vite_plugin_react_preamble_installed__: true,
    $RefreshSig$: () => (type: unknown) => type,
    $RefreshReg$: () => {},
  });
});
const api = vi.hoisted(() => ({ confirmEmail: vi.fn() }));
vi.mock("@/data/lib/account/notifications", async (importActual) => ({
  ...(await importActual<typeof import("@/data/lib/account/notifications")>()),
  confirmEmail: api.confirmEmail,
}));
vi.mock("./TurnstileWidget", () => ({
  TurnstileWidget: ({ onToken }: { onToken: (token: string) => void }) => (
    <button type="button" onClick={() => onToken("token-1")}>
      Solve challenge
    </button>
  ),
}));

import { CatalystError } from "@/data/lib/catalyst/client";

import { EmailConfirmationRoute, type EmailConfirmationRouteProps } from "./EmailConfirmationRoute";

const ADDRESS = "0x4e9c4a2502fdf71e93ed8ed6ca9ddbd891d6f295";
const CODE = "a".repeat(32);

describe("EmailConfirmationRoute", () => {
  let client: QueryClient;
  let props: EmailConfirmationRouteProps;

  function renderRoute() {
    render(
      <QueryClientProvider client={client}>
        <EmailConfirmationRoute {...props} />
      </QueryClientProvider>,
    );
  }

  beforeEach(() => {
    client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  });

  afterEach(() => {
    cleanup();
    client.clear();
    vi.resetAllMocks();
  });

  describe("when the link has no address", () => {
    beforeEach(() => {
      props = { code: CODE, address: null, source: "account", turnstileSiteKey: null, mode: "challenge" };
      renderRoute();
    });

    it("should show the missing address page", () => {
      expect(screen.getByRole("heading", { level: 1, name: "Missing Address" })).toBeTruthy();
    });
  });

  describe("when the link has an unknown source", () => {
    beforeEach(() => {
      props = { code: CODE, address: ADDRESS, source: "shop", turnstileSiteKey: null, mode: "challenge" };
      renderRoute();
    });

    it("should show the invalid source page", () => {
      expect(screen.getByRole("heading", { level: 1, name: "Invalid Source" })).toBeTruthy();
    });
  });

  describe("when the challenge page has a site key", () => {
    beforeEach(() => {
      props = { code: CODE, address: ADDRESS, source: "credits", turnstileSiteKey: "site-key", mode: "challenge" };
    });

    describe("and the challenge is not solved", () => {
      beforeEach(() => {
        renderRoute();
      });

      it("should keep the confirm button disabled", () => {
        expect((screen.getByRole("button", { name: "Confirm Email" }) as HTMLButtonElement).disabled).toBe(true);
      });
    });

    describe("and the member solves it and confirms", () => {
      beforeEach(async () => {
        api.confirmEmail.mockResolvedValueOnce(undefined);
        renderRoute();
        await userEvent.click(screen.getByRole("button", { name: "Solve challenge" }));
        await userEvent.click(screen.getByRole("button", { name: "Confirm Email" }));
      });

      it("should send the address, code, source and challenge token", () => {
        expect(api.confirmEmail).toHaveBeenCalledWith(
          { address: ADDRESS, code: CODE, source: "credits", turnstileToken: "token-1" },
          { signal: expect.any(AbortSignal) },
        );
      });

      it("should show the confirmation with a link to the Shop", async () => {
        expect((await screen.findByRole("link", { name: "Go to Shop" })).getAttribute("href")).toBe("/shop");
      });
    });

    describe("and the node refuses the captcha", () => {
      beforeEach(async () => {
        api.confirmEmail.mockRejectedValueOnce(new CatalystError("Invalid captcha", "u", 401, true));
        renderRoute();
        await userEvent.click(screen.getByRole("button", { name: "Solve challenge" }));
        await userEvent.click(screen.getByRole("button", { name: "Confirm Email" }));
      });

      it("should ask for the challenge again", async () => {
        expect((await screen.findByRole("alert")).textContent).toContain("security check did not pass");
      });

      it("should disable the confirm button until a new token arrives", async () => {
        await screen.findByRole("alert");
        expect((screen.getByRole("button", { name: "Confirm Email" }) as HTMLButtonElement).disabled).toBe(true);
      });
    });

    describe("and the node refuses the code", () => {
      beforeEach(async () => {
        api.confirmEmail.mockRejectedValueOnce(new CatalystError("Invalid code", "u", 400, true));
        renderRoute();
        await userEvent.click(screen.getByRole("button", { name: "Solve challenge" }));
        await userEvent.click(screen.getByRole("button", { name: "Confirm Email" }));
      });

      it("should show the failure page without a confirm button", async () => {
        await screen.findByRole("heading", { level: 1, name: "We Couldn't Confirm Your Email" });
        expect(screen.queryByRole("button", { name: "Confirm Email" })).toBeNull();
      });
    });
  });

  describe("when the link confirms on load", () => {
    beforeEach(() => {
      props = { code: CODE, address: ADDRESS, source: "account", turnstileSiteKey: null, mode: "auto" };
    });

    describe("and the node accepts it", () => {
      beforeEach(() => {
        api.confirmEmail.mockResolvedValueOnce(undefined);
        renderRoute();
      });

      it("should confirm once, with a time budget and without a challenge token", async () => {
        await waitFor(() =>
          expect(api.confirmEmail.mock.calls).toEqual([
            [{ address: ADDRESS, code: CODE, source: "account" }, { signal: expect.any(AbortSignal) }],
          ]),
        );
      });

      it("should show the confirmation with a link back to Decentraland", async () => {
        expect((await screen.findByRole("link", { name: "Go to Decentraland" })).getAttribute("href")).toBe("/");
      });
    });

    describe("and the node requires a challenge the site cannot show", () => {
      beforeEach(() => {
        api.confirmEmail.mockRejectedValueOnce(new CatalystError("Invalid captcha", "u", 401, true));
        renderRoute();
      });

      it("should report the misconfiguration without a retry button", async () => {
        expect((await screen.findByRole("alert")).textContent).toContain("not set up to show");
        expect(screen.queryByRole("button", { name: "Confirm Email" })).toBeNull();
      });
    });

    describe("and no email is pending", () => {
      beforeEach(() => {
        api.confirmEmail.mockRejectedValueOnce(
          new CatalystError("No unconfirmed email for this address", "u", 400, true),
        );
        renderRoute();
      });

      it("should explain that nothing is waiting for confirmation", async () => {
        expect((await screen.findByRole("alert")).textContent).toContain("no email waiting for confirmation");
      });
    });
  });
});

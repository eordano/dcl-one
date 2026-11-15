// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

vi.hoisted(() => {
  Object.assign(window, { __vite_plugin_react_preamble_installed__: true });
});
const magic = vi.hoisted(() => ({ key: "pk_example123", start: vi.fn(async () => {}), remember: vi.fn() }));
const auth = vi.hoisted(() => ({
  connect: vi.fn(async () => ({ signer: "0x0000000000000000000000000000000000000001" })),
  verifyEmailSignIn: vi.fn(async () => ({ signer: "0x0000000000000000000000000000000000000001" })),
}));
vi.mock("@/data/lib/auth/magic-config", () => ({ magicApiKey: () => magic.key }));
vi.mock("../../auth/login/magic", () => ({ startMagicSignIn: magic.start }));
vi.mock("../../auth/login/remember", () => ({ rememberEmail: magic.remember, SESSION_MS: 2_592_000_000 }));
vi.mock("@/data/lib/auth/index", () => ({
  useAuth: () => ({
    isConnected: false,
    inAppAvailable: true,
    walletAvailable: true,
    detectedWallets: [],
    error: null,
    startEmailSignIn: vi.fn(async () => {}),
    connect: auth.connect,
    verifyEmailSignIn: auth.verifyEmailSignIn,
  }),
}));

import SignInModalHost from "./SignInModalHost";
import { closeSignIn, openSignIn, peekPendingRedirect } from "./signin-store";

// The host loads the view lazily; a first load on a busy machine outlasts the query's wait.
beforeAll(async () => {
  await import("@ui/components/SignInModalView");
}, 60_000);
beforeEach(() => {
  vi.stubGlobal("location", { pathname: "/create/storage", search: "?step=edit", hash: "", origin: "http://localhost:3000", assign: vi.fn() });
});
afterEach(() => {
  cleanup();
  closeSignIn();
  vi.unstubAllGlobals();
  vi.clearAllMocks();
  magic.key = "pk_example123";
});

async function openModal() {
  render(
    <MemoryRouter>
      <SignInModalHost />
    </MemoryRouter>,
  );
  await act(async () => {
    openSignIn({ inPlace: true });
  });
}

describe("the in-place sign-in modal", () => {
  it("signs in with Google through Magic, back to this page, as /auth/login does", async () => {
    await openModal();
    fireEvent.click(await screen.findByRole("button", { name: "Continue with Google" }));
    await vi.waitFor(() => expect(magic.start).toHaveBeenCalledWith("pk_example123", "google", "/create/storage?step=edit"));
  });

  it("starts one social sign-in however often it is pressed", async () => {
    magic.start.mockImplementation(() => new Promise(() => {}));
    await openModal();
    const google = await screen.findByRole("button", { name: "Continue with Google" });
    await act(async () => {
      fireEvent.click(google);
      fireEvent.click(google);
    });
    expect(magic.start).toHaveBeenCalledOnce();
  });

  it("starts each opening clean: no earlier failure, social sign-in available again", async () => {
    magic.start.mockRejectedValueOnce(new Error("Magic is unreachable"));
    await openModal();
    await act(async () => {
      fireEvent.click(await screen.findByRole("button", { name: "Continue with Google" }));
    });
    expect(await screen.findByText("Magic is unreachable")).toBeTruthy();
    await act(async () => closeSignIn());
    await act(async () => openSignIn({ inPlace: true }));
    expect(screen.queryByText("Magic is unreachable")).toBeNull();

    magic.start.mockImplementationOnce(() => new Promise(() => {}));
    await act(async () => {
      fireEvent.click(await screen.findByRole("button", { name: "Continue with Google" }));
    });
    expect(magic.start).toHaveBeenCalledTimes(2);
    // Back from the provider through the back/forward cache: the start in flight no longer counts.
    await act(async () => {
      window.dispatchEvent(Object.assign(new Event("pageshow"), { persisted: true }));
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Continue with Google" }));
    });
    expect(magic.start).toHaveBeenCalledTimes(3);
  });

  it("says when social sign-in cannot start, keeps the page to return to, and lets the reader try again", async () => {
    await act(async () => {
      openSignIn({ inPlace: true, redirectTo: "/create/storage?step=pay" });
    });
    render(
      <MemoryRouter>
        <SignInModalHost />
      </MemoryRouter>,
    );
    magic.start.mockRejectedValueOnce(new Error("Magic is unreachable"));
    await act(async () => {
      fireEvent.click(await screen.findByRole("button", { name: "Continue with Google" }));
    });
    expect(await screen.findByText("Magic is unreachable")).toBeTruthy();
    expect(peekPendingRedirect()).toBe("/create/storage?step=pay");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Continue with Google" }));
    });
    expect(magic.start).toHaveBeenCalledTimes(2);
    expect(magic.start).toHaveBeenLastCalledWith("pk_example123", "google", "/create/storage?step=pay");
  });

  it("signs a wallet session for a month, as /auth/login does, and keeps no email for it", async () => {
    await openModal();
    await act(async () => {
      fireEvent.click(await screen.findByRole("button", { name: "Continue with wallet" }));
    });
    await act(async () => {
      fireEvent.click(await screen.findByRole("button", { name: "Use a browser wallet" }));
    });
    expect(auth.connect).toHaveBeenCalledWith(expect.objectContaining({ expirationMs: 2_592_000_000 }));
    expect(magic.remember).toHaveBeenCalledWith(null);
  });

  it("signs an email session for a month and keeps the email for profile setup", async () => {
    await openModal();
    fireEvent.change(await screen.findByRole("textbox", { name: "Email" }), { target: { value: "jane.doe@example.com" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Continue with email" }));
    });
    fireEvent.change(await screen.findByLabelText("Verification code"), { target: { value: "123456" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Verify & sign in" }));
    });
    expect(auth.verifyEmailSignIn).toHaveBeenCalledWith("jane.doe@example.com", "123456", { expirationMs: 2_592_000_000 });
    expect(magic.remember).toHaveBeenCalledWith("jane.doe@example.com");
  });

  it("offers no social option where Magic is not configured, rather than a different wallet", async () => {
    magic.key = "";
    await openModal();
    await screen.findByRole("dialog");
    expect(screen.queryByRole("button", { name: "Continue with Google" })).toBeNull();
  });
});

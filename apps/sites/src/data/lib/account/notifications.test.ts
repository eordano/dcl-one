import { afterEach, beforeEach, describe, expect, it, vi, type MockInstance } from "vitest";

import { CatalystError } from "../catalyst/client";
import {
  confirmEmail,
  emailConfirmationFailure,
  EmailConfirmationTimeout,
  type EmailConfirmation,
} from "./notifications";

const ADDRESS = "0x4e9c4a2502fdf71e93ed8ed6ca9ddbd891d6f295";
const BASE = "https://node.example.test";

describe("confirmEmail", () => {
  let fetchSpy: MockInstance<typeof fetch>;
  let confirmation: EmailConfirmation;

  beforeEach(() => {
    fetchSpy = vi.spyOn(globalThis, "fetch");
    confirmation = { address: ADDRESS, code: "a".repeat(32), source: "credits", turnstileToken: "token-1" };
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  describe("when the node accepts the code", () => {
    beforeEach(() => {
      fetchSpy.mockResolvedValueOnce(new Response(null, { status: 204 }));
    });

    it("should PUT the confirmation, unsigned, to the notifications confirm-email route", async () => {
      await confirmEmail(confirmation, { base: BASE });

      const [url, init] = fetchSpy.mock.calls[0] as [string, RequestInit];
      expect({ url, method: init.method, body: JSON.parse(String(init.body)) }).toEqual({
        url: `${BASE}/notifications/confirm-email`,
        method: "PUT",
        body: { address: ADDRESS, code: "a".repeat(32), source: "credits", turnstileToken: "token-1" },
      });
    });
  });

  describe("when the node refuses the code", () => {
    beforeEach(() => {
      fetchSpy.mockResolvedValueOnce(
        new Response(JSON.stringify({ error: "Bad request", message: "Invalid code" }), { status: 400 }),
      );
    });

    it("should reject with the node's message and status", async () => {
      await expect(confirmEmail(confirmation, { base: BASE })).rejects.toMatchObject({
        status: 400,
        message: "Invalid code",
        serverMessage: true,
      });
    });
  });

  describe("when the request times out", () => {
    beforeEach(() => {
      fetchSpy.mockRejectedValueOnce(new DOMException("The operation timed out.", "TimeoutError"));
    });

    it("should reject with a confirmation timeout", async () => {
      await expect(confirmEmail(confirmation, { base: BASE })).rejects.toBeInstanceOf(EmailConfirmationTimeout);
    });
  });

  describe("when the node cannot be reached", () => {
    beforeEach(() => {
      fetchSpy.mockRejectedValueOnce(new TypeError("network down"));
    });

    it("should reject with a request failure that carries no status", async () => {
      await expect(confirmEmail(confirmation, { base: BASE })).rejects.toMatchObject({ status: 0 });
    });
  });
});

describe("emailConfirmationFailure", () => {
  let error: unknown;

  describe("when the captcha was refused", () => {
    beforeEach(() => {
      error = new CatalystError("Invalid captcha", "u", 401, true);
    });

    describe("and the page showed the challenge", () => {
      it("should ask for the challenge again and allow a retry", () => {
        expect(emailConfirmationFailure(error, { challengeShown: true })).toEqual({
          message: "The security check did not pass. Complete it again and retry.",
          retry: true,
        });
      });
    });

    describe("and the page had no challenge to show", () => {
      it("should report the missing challenge without a retry", () => {
        expect(emailConfirmationFailure(error, { challengeShown: false })).toMatchObject({
          retry: false,
          message: expect.stringContaining("not set up to show"),
        });
      });
    });
  });

  describe("when the request timed out", () => {
    beforeEach(() => {
      error = new EmailConfirmationTimeout("u");
    });

    it("should say it took too long and allow a retry", () => {
      expect(emailConfirmationFailure(error, { challengeShown: false })).toEqual({
        message: "The confirmation is taking too long. Try again in a moment.",
        retry: true,
      });
    });
  });

  describe("when the code does not match", () => {
    beforeEach(() => {
      error = new CatalystError("Invalid code", "u", 400, true);
    });

    it("should report an invalid link that cannot be retried", () => {
      expect(emailConfirmationFailure(error, { challengeShown: true })).toMatchObject({ retry: false, message: expect.stringContaining("invalid") });
    });
  });

  describe("when no email is pending", () => {
    beforeEach(() => {
      error = new CatalystError("No unconfirmed email for this address", "u", 400, true);
    });

    it("should say nothing is waiting for confirmation", () => {
      expect(emailConfirmationFailure(error, { challengeShown: true })).toMatchObject({
        retry: false,
        message: expect.stringContaining("no email waiting for confirmation"),
      });
    });
  });

  describe("when the email's domain is refused", () => {
    beforeEach(() => {
      error = new CatalystError("Email address invalid. Please use a functioning email to continue", "u", 400, true);
    });

    it("should ask for a different address", () => {
      expect(emailConfirmationFailure(error, { challengeShown: true })).toMatchObject({
        retry: false,
        message: expect.stringContaining("can't be used"),
      });
    });
  });

  describe("when the node failed without a message", () => {
    beforeEach(() => {
      error = new CatalystError("Catalyst returned 502 Bad Gateway", "u", 502);
    });

    it("should allow a retry", () => {
      expect(emailConfirmationFailure(error, { challengeShown: true }).retry).toBe(true);
    });
  });
});

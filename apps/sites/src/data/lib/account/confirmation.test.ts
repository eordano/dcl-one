import { describe, expect, it, beforeEach } from "vitest";

import { accountHostRedirect, legacyConfirmationRedirect, turnstileSiteKey } from "./confirmation";

describe("accountHostRedirect", () => {
  let request: Request;

  describe("when the page was reached on the account subdomain", () => {
    beforeEach(() => {
      request = new Request("http://account.example.test/account/confirm-email-challenge/abc?address=0x1&source=account", {
        headers: { "x-forwarded-proto": "https" },
      });
    });

    it("should move to the same page on the apex host, keeping the query and the forwarded scheme", () => {
      expect(accountHostRedirect(request)).toBe(
        "https://example.test/account/confirm-email-challenge/abc?address=0x1&source=account",
      );
    });
  });

  describe("when the page was reached on the apex host", () => {
    beforeEach(() => {
      request = new Request("https://example.test/account/confirm-email-challenge/abc");
    });

    it("should not redirect", () => {
      expect(accountHostRedirect(request)).toBeNull();
    });
  });
});

describe("legacyConfirmationRedirect", () => {
  let request: Request;

  describe("when the link names no source", () => {
    beforeEach(() => {
      request = new Request("https://example.test/account/credits-email-confirmed/abc?address=0x1");
    });

    it("should point at the challenge page with the route's source", () => {
      expect(legacyConfirmationRedirect(request, "abc", "credits")).toBe(
        "/account/confirm-email-challenge/abc?address=0x1&source=credits",
      );
    });
  });

  describe("when the link already names a source", () => {
    beforeEach(() => {
      request = new Request("https://example.test/account/confirm-email/abc?address=0x1&source=account");
    });

    it("should keep it", () => {
      expect(legacyConfirmationRedirect(request, "abc", "credits")).toBe(
        "/account/confirm-email-challenge/abc?address=0x1&source=account",
      );
    });
  });
});

describe("turnstileSiteKey", () => {
  let env: Record<string, string | undefined>;

  describe("when the key is set", () => {
    beforeEach(() => {
      env = { TURNSTILE_SITE_KEY: " 0x-example-site-key " };
    });

    it("should return it trimmed", () => {
      expect(turnstileSiteKey(env)).toBe("0x-example-site-key");
    });
  });

  describe("when the key is blank", () => {
    beforeEach(() => {
      env = { TURNSTILE_SITE_KEY: " " };
    });

    it("should report the challenge as off", () => {
      expect(turnstileSiteKey(env)).toBeNull();
    });
  });
});

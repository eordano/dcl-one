import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { loader as challengeLoader } from "./account.confirm-email-challenge.$code";
import { loader as accountLinkLoader } from "./account.confirm-email.$code";
import { loader as creditsLinkLoader } from "./account.credits-email-confirmed.$code";

const CODE = "a".repeat(32);
const ADDRESS = "0x" + "ab".repeat(20);

type Loader = (args: never) => unknown;

const args = (url: string, headers: Record<string, string> = {}) =>
  ({ request: new Request(url, { headers }), params: { code: CODE }, context: {} }) as never;

function redirectOf(load: Loader, url: string, headers?: Record<string, string>) {
  try {
    load(args(url, headers));
  } catch (thrown) {
    expect(thrown).toBeInstanceOf(Response);
    return (thrown as Response).headers.get("location");
  }
  return null;
}

describe("account confirmation route loaders", () => {
  afterEach(() => {
    vi.unstubAllEnvs();
  });

  describe("when a page is requested on the account subdomain", () => {
    let url: string;

    beforeEach(() => {
      url = `http://account.example.test/account/confirm-email-challenge/${CODE}?address=${ADDRESS}&source=account`;
    });

    it("should redirect the challenge page to the apex host", () => {
      expect(redirectOf(challengeLoader, url, { "x-forwarded-proto": "https" })).toBe(
        `https://example.test/account/confirm-email-challenge/${CODE}?address=${ADDRESS}&source=account`,
      );
    });

    it("should redirect a legacy link to the apex host before anything else", () => {
      vi.stubEnv("TURNSTILE_SITE_KEY", "site-key");
      expect(redirectOf(accountLinkLoader, url.replace("confirm-email-challenge", "confirm-email"))).toBe(
        `http://example.test/account/confirm-email/${CODE}?address=${ADDRESS}&source=account`,
      );
    });
  });

  describe("when the challenge page is requested on the apex host", () => {
    beforeEach(() => {
      vi.stubEnv("TURNSTILE_SITE_KEY", "site-key");
    });

    it("should hand the link's fields and the site key to the page", () => {
      expect(
        challengeLoader(
          args(`https://example.test/account/confirm-email-challenge/${CODE}?address=${ADDRESS}&source=credits`),
        ),
      ).toEqual({ code: CODE, address: ADDRESS, source: "credits", turnstileSiteKey: "site-key" });
    });
  });

  describe("when a legacy link is opened", () => {
    describe("and the site has a Turnstile key", () => {
      beforeEach(() => {
        vi.stubEnv("TURNSTILE_SITE_KEY", "site-key");
      });

      it("should move the account link to the challenge page with the account source", () => {
        expect(redirectOf(accountLinkLoader, `https://example.test/account/confirm-email/${CODE}?address=${ADDRESS}`)).toBe(
          `/account/confirm-email-challenge/${CODE}?address=${ADDRESS}&source=account`,
        );
      });

      it("should move the credits link to the challenge page with the credits source", () => {
        expect(
          redirectOf(creditsLinkLoader, `https://example.test/account/credits-email-confirmed/${CODE}?address=${ADDRESS}`),
        ).toBe(`/account/confirm-email-challenge/${CODE}?address=${ADDRESS}&source=credits`);
      });
    });

    describe("and the site has no Turnstile key", () => {
      beforeEach(() => {
        vi.stubEnv("TURNSTILE_SITE_KEY", "");
      });

      it("should render the credits page to confirm on load", () => {
        expect(
          creditsLinkLoader(args(`https://example.test/account/credits-email-confirmed/${CODE}?address=${ADDRESS}`)),
        ).toEqual({ code: CODE, address: ADDRESS, source: "credits", turnstileSiteKey: null });
      });
    });
  });
});

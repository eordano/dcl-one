import type { EmailConfirmationSource } from "./notifications";

export const ACCOUNT_BASE = "/account";
const ACCOUNT_HOST = "account.";

/** Where an emailed confirmation link lands on this site. */
export function confirmationChallengePath(code: string, query: URLSearchParams): string {
  const qs = query.toString();
  return `${ACCOUNT_BASE}/confirm-email-challenge/${encodeURIComponent(code)}${qs ? `?${qs}` : ""}`;
}

/**
 * The gateway serves `account.<domain>/<path>` as `/account/<path>`, which the browser router cannot
 * match, so such a request moves to the same page on `<domain>`.
 */
export function accountHostRedirect(request: Request): string | null {
  const url = new URL(request.url);
  if (!url.hostname.startsWith(ACCOUNT_HOST) || url.hostname === ACCOUNT_HOST.slice(0, -1)) return null;
  const scheme = request.headers.get("x-forwarded-proto");
  if (scheme === "https" || scheme === "http") url.protocol = `${scheme}:`;
  url.hostname = url.hostname.slice(ACCOUNT_HOST.length);
  return url.href;
}

/** The legacy links (`/confirm-email`, `/credits-email-confirmed`) move to the challenge page when it is on. */
export function legacyConfirmationRedirect(
  request: Request,
  code: string,
  source: EmailConfirmationSource,
): string {
  const query = new URL(request.url).searchParams;
  if (!query.has("source")) query.set("source", source);
  return confirmationChallengePath(code, query);
}

/** Cloudflare Turnstile's public site key; unset runs the pages without the challenge. */
export function turnstileSiteKey(env: Record<string, string | undefined> = process.env): string | null {
  return env.TURNSTILE_SITE_KEY?.trim() || null;
}

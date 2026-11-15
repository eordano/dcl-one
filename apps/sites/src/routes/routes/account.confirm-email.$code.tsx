import "@dcl/one-ui/foundations/foundations.css";
import { redirect } from "react-router";

import { accountHostRedirect, legacyConfirmationRedirect, turnstileSiteKey } from "@/data/lib/account/confirmation";
import { EmailConfirmationRoute } from "@/features/account/EmailConfirmationRoute";

import type { Route } from "./+types/account.confirm-email.$code";

export function meta() {
  return [{ title: "Confirm Your Email | Decentraland" }, { name: "robots", content: "noindex" }];
}

/** The link sent without a challenge; with a site key it moves to the challenge page. */
export function loader({ request, params }: Route.LoaderArgs) {
  const moved = accountHostRedirect(request);
  if (moved) throw redirect(moved);
  if (turnstileSiteKey()) throw redirect(legacyConfirmationRedirect(request, params.code, "account"));
  return {
    code: params.code,
    address: new URL(request.url).searchParams.get("address"),
    source: "account",
    turnstileSiteKey: null,
  };
}

export default function ConfirmEmailLink({ loaderData }: Route.ComponentProps) {
  return <EmailConfirmationRoute {...loaderData} mode="auto" />;
}

import "@dcl/one-ui/foundations/foundations.css";
import { redirect } from "react-router";

import { accountHostRedirect, turnstileSiteKey } from "@/data/lib/account/confirmation";
import { EmailConfirmationRoute } from "@/features/account/EmailConfirmationRoute";

import type { Route } from "./+types/account.confirm-email-challenge.$code";

export function meta() {
  return [{ title: "Confirm Your Email | Decentraland" }, { name: "robots", content: "noindex" }];
}

export function loader({ request, params }: Route.LoaderArgs) {
  const moved = accountHostRedirect(request);
  if (moved) throw redirect(moved);
  const query = new URL(request.url).searchParams;
  return {
    code: params.code,
    address: query.get("address"),
    source: query.get("source"),
    turnstileSiteKey: turnstileSiteKey(),
  };
}

export default function ConfirmEmailChallenge({ loaderData }: Route.ComponentProps) {
  return <EmailConfirmationRoute {...loaderData} mode="challenge" />;
}

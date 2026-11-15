import { z } from "zod";

import type { AuthIdentity } from "../auth/types";
import { NOTIFICATIONS_PREFIX } from "../catalyst/bases";
import { CatalystError, catalystBase, errorFromResponse, postJSON, signedGetJSON } from "../catalyst/client";
import { SubscriptionSchema } from "../catalyst/generated-schemas/notifications";

const BASE = NOTIFICATIONS_PREFIX;

const ChannelSchema = z.object({ email: z.boolean(), in_app: z.boolean() });

const SubscriptionDetailsSchema = z.object({
  ignore_all_email: z.boolean(),
  ignore_all_in_app: z.boolean(),
  message_type: z.record(z.string(), ChannelSchema),
});

/** The generated reply leaves `details` open; the settings page needs its shape. */
const SubscriptionResponseSchema = SubscriptionSchema.extend({ details: SubscriptionDetailsSchema });

export type SubscriptionDetails = z.infer<typeof SubscriptionDetailsSchema>;
export type SubscriptionResponse = z.infer<typeof SubscriptionResponseSchema>;

/** The caller's own subscription: the signed request names the account, not the path. */
export async function fetchSubscription(
  identity: AuthIdentity,
  opts: { signal?: AbortSignal } = {},
): Promise<SubscriptionResponse> {
  const raw = await signedGetJSON<unknown>(`${BASE}/subscription`, { identity, signal: opts.signal });
  return SubscriptionResponseSchema.parse(raw);
}

export async function putSubscription(
  identity: AuthIdentity,
  details: SubscriptionDetails,
  opts: { signal?: AbortSignal } = {},
): Promise<void> {
  await postJSON<unknown>(`${BASE}/subscription`, details, {
    identity,
    method: "PUT",
    signal: opts.signal,
  });
}

/** Starts an address change; the server mails a confirmation link to `email`. */
export async function putEmail(
  identity: AuthIdentity,
  email: string,
  opts: { signal?: AbortSignal } = {},
): Promise<void> {
  await postJSON<unknown>(`${BASE}/set-email`, { email }, {
    identity,
    method: "PUT",
    signal: opts.signal,
  });
}

export type SubscriptionGroupKey =
  | "marketplace"
  | "worlds"
  | "credits"
  | "streaming"
  | "events"
  | "rewards"
  | "dao"
  | "tips"
  | "referral";

/** Group to notification types, as the account settings list them (source `account-notifications.helpers.ts`). */
export const SUBSCRIPTION_GROUPS: Record<SubscriptionGroupKey, readonly string[]> = {
  marketplace: [
    "item_sold",
    "bid_accepted",
    "bid_received",
    "royalties_earned",
    "rental_ended",
    "rental_started",
  ],
  worlds: [
    "worlds_access_restored",
    "worlds_access_restricted",
    "worlds_missing_resources",
    "worlds_permission_granted",
    "worlds_permission_revoked",
  ],
  credits: [
    "credits_reminder_complete_goals",
    "credits_reminder_claim_credits",
    "credits_reminder_usage",
    "credits_reminder_do_not_miss_out",
  ],
  streaming: [
    "streaming_key_expired",
    "streaming_key_reset",
    "streaming_key_revoke",
    "streaming_place_updated",
    "streaming_time_exceeded",
  ],
  events: ["events_started", "events_starts_soon"],
  rewards: ["reward_assignment", "reward_campaign_out_of_funds", "reward_campaign_out_of_stock"],
  dao: [
    "governance_announcement",
    "governance_authored_proposal_finished",
    "governance_coauthor_requested",
    "governance_new_comment_on_project_update",
    "governance_new_comment_on_proposal",
    "governance_proposal_enacted",
    "governance_voting_ended_voter",
  ],
  tips: ["tip_received"],
  referral: ["referral_invited_users_accepted", "referral_new_tier_reached"],
};

/** Two independent columns on desktop; mobile reads them in this order as one. */
export const SUBSCRIPTION_GROUP_COLUMNS: readonly (readonly SubscriptionGroupKey[])[] = [
  ["marketplace", "credits", "events", "rewards", "dao"],
  ["worlds", "streaming", "tips", "referral"],
];

export function isTypeEmailEnabled(details: SubscriptionDetails, type: string): boolean {
  return details.message_type[type]?.email === true;
}

/**
 * Sets one type's email channel, leaving in-app as it was (on for a type not yet listed).
 * `ignore_all_email` becomes true only when no type keeps email on.
 */
export function setTypeEmail(
  details: SubscriptionDetails,
  type: string,
  enabled: boolean,
): SubscriptionDetails {
  const current = details.message_type[type] ?? { email: false, in_app: true };
  const messageType = { ...details.message_type, [type]: { ...current, email: enabled } };
  const ignoreAllEmail = Object.values(messageType).every((channel) => !channel.email);
  return { ...details, ignore_all_email: ignoreAllEmail, message_type: messageType };
}

export function isAllEmailEnabled(details: SubscriptionDetails): boolean {
  return !details.ignore_all_email;
}

/** The master switch only mutes or unmutes email; each type keeps its own setting. */
export function setAllEmail(details: SubscriptionDetails, enabled: boolean): SubscriptionDetails {
  return { ...details, ignore_all_email: !enabled };
}

export type EmailConfirmationSource = "account" | "credits";

export function isEmailConfirmationSource(value: string | null | undefined): value is EmailConfirmationSource {
  return value === "account" || value === "credits";
}

export type EmailConfirmation = {
  address: string;
  code: string;
  source: EmailConfirmationSource;
  turnstileToken?: string;
};

/** Confirms a pending address with the emailed code. Unsigned: the link may open in any browser. */
export async function confirmEmail(
  confirmation: EmailConfirmation,
  opts: { signal?: AbortSignal; base?: string } = {},
): Promise<void> {
  const url = `${catalystBase(opts.base)}${BASE}/confirm-email`;
  let res: Response;
  try {
    res = await fetch(url, {
      method: "PUT",
      headers: { accept: "application/json", "content-type": "application/json" },
      body: JSON.stringify(confirmation),
      signal: opts.signal,
    });
  } catch (err) {
    if ((err as Error)?.name === "TimeoutError") throw new EmailConfirmationTimeout(url);
    throw new CatalystError(`Catalyst request failed: ${(err as Error)?.message ?? "network error"}`, url);
  }
  if (!res.ok) throw await errorFromResponse(res, url);
  await res.body?.cancel().catch(() => {});
}

/** The confirmation got no answer within its time budget. */
export class EmailConfirmationTimeout extends CatalystError {
  constructor(url: string) {
    super("Email confirmation timed out", url);
    this.name = "EmailConfirmationTimeout";
  }
}

export type EmailConfirmationFailure = { message: string; retry: boolean };

const INVALID_LINK = "This confirmation link is invalid or was replaced by a newer one. Request a new link from your account settings.";

/**
 * The node's refusals, worded for the page; only a failed challenge, a timeout or an outage can be
 * retried. A refused captcha with no challenge shown means the node checks one this site cannot show.
 */
export function emailConfirmationFailure(
  error: unknown,
  { challengeShown }: { challengeShown: boolean },
): EmailConfirmationFailure {
  if (error instanceof EmailConfirmationTimeout) {
    return { message: "The confirmation is taking too long. Try again in a moment.", retry: true };
  }
  const server = error instanceof CatalystError && error.serverMessage ? error.message : "";
  if (server === "Invalid captcha") {
    return challengeShown
      ? { message: "The security check did not pass. Complete it again and retry.", retry: true }
      : {
          message: "Email confirmation needs a security check this site is not set up to show. Please contact support.",
          retry: false,
        };
  }
  if (server === "Invalid code" || server === "Missing code" || server === "Missing address") {
    return { message: INVALID_LINK, retry: false };
  }
  if (server === "No unconfirmed email for this address") {
    return { message: "There is no email waiting for confirmation on this account. It may already be confirmed.", retry: false };
  }
  if (server.startsWith("Email address invalid")) {
    return { message: "This email address can't be used. Set a different one from your account settings.", retry: false };
  }
  return { message: "We couldn't confirm your email right now. Try again in a moment.", retry: true };
}

import {
  skipToken,
  useMutation,
  useMutationState,
  useQuery,
  useQueryClient,
  type Mutation,
  type MutationFilters,
  type MutationStatus,
  type QueryClient,
} from "@tanstack/react-query";

import type { AuthIdentity } from "@/data/lib/auth/types";
import { optOutOfCredits } from "@/data/lib/account/credits";
import {
  confirmEmail,
  putEmail,
  putSubscription,
  type EmailConfirmation,
  type SubscriptionDetails,
} from "@/data/lib/account/notifications";
import {
  ACCOUNT_STALE_MS,
  accountKeys,
  creditsStatusQuery,
  subscriptionQuery,
} from "@/data/lib/account/queries";

/*
 * Every write takes the signing identity as a variable, not from the hook's props. A pending
 * mutation picks up the latest options on re-render, so after an account switch a callback that
 * read the props would sign or write as the new account. The variables stay the ones it started with.
 */

export type SubscriptionUpdate = { identity: AuthIdentity; details: SubscriptionDetails };
const UPDATE_SUBSCRIPTION = ["account", "update-subscription"] as const;

/** A write with no reply could otherwise hold every later write in its queue. */
export const SUBSCRIPTION_WRITE_TIMEOUT_MS = 15_000;

const writesOf = (account: string): MutationFilters => ({
  mutationKey: UPDATE_SUBSCRIPTION,
  predicate: (mutation: Mutation<unknown, Error, unknown, unknown>) =>
    (mutation.state.variables as SubscriptionUpdate | undefined)?.identity.signer.toLowerCase() === account,
});

function pendingWritesOf(client: QueryClient, account: string) {
  return client.isMutating(writesOf(account));
}

/**
 * The subscription as the member sees it: the server's reply, with the details of the newest write
 * still pending, or else of the newest write the server accepted after that reply was read. Failed
 * writes show nothing, so the server's or the last accepted details come back without a rollback,
 * and a re-read while a write is pending cannot hide it.
 */
export function useSubscription(identity: AuthIdentity | null) {
  const account = identity?.signer.toLowerCase() ?? "";
  const query = useQuery(
    identity
      ? subscriptionQuery(identity)
      : { queryKey: accountKeys.subscription(""), queryFn: skipToken, staleTime: ACCOUNT_STALE_MS },
  );
  const writes = useMutationState<{ status: MutationStatus; submittedAt: number; details: SubscriptionDetails }>({
    filters: writesOf(account),
    select: (mutation) => ({
      status: mutation.state.status,
      submittedAt: mutation.state.submittedAt,
      details: (mutation.state.variables as SubscriptionUpdate).details,
    }),
  });
  const pending = writes.filter((write) => write.status === "pending").at(-1);
  const accepted = writes
    .filter((write) => write.status === "success" && write.submittedAt > query.dataUpdatedAt)
    .at(-1);
  const shown = pending ?? accepted;
  if (!identity || !query.data || !shown) return query;
  return { ...query, data: { ...query.data, details: shown.details } };
}

/**
 * Sends the whole details object. The writes run one at a time (`scope`), so a later toggle cannot
 * land before an earlier one and lose it; `useSubscription` shows the pending details meanwhile. The
 * scope is shared by every account because it is fixed when the hook is created, not per call; the
 * timeout bounds how long another account's write can hold the queue. The PUT reply is not read
 * back, since the server can answer with partial details; once the last pending write of the account
 * settles the subscription is read again.
 */
export function useUpdateSubscription() {
  const client = useQueryClient();
  return useMutation({
    mutationKey: UPDATE_SUBSCRIPTION,
    scope: { id: "account-subscription" },
    mutationFn: ({ identity, details }: SubscriptionUpdate) =>
      putSubscription(identity, details, { signal: AbortSignal.timeout(SUBSCRIPTION_WRITE_TIMEOUT_MS) }),
    onSettled: (_result, _error, { identity }) => {
      const account = identity.signer.toLowerCase();
      if (pendingWritesOf(client, account) > 1) return;
      return client.invalidateQueries({ queryKey: accountKeys.subscription(account) });
    },
  });
}

/**
 * The server decides whether a confirmation is pending: the same address sends none, an empty one
 * clears it, and the input is trimmed. So the subscription is read again instead of guessed.
 */
export function useSetEmail() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ identity, email }: { identity: AuthIdentity; email: string }) =>
      putEmail(identity, email),
    onSuccess: (_result, { identity }) =>
      client.invalidateQueries({ queryKey: accountKeys.subscription(identity.signer) }),
  });
}

export function useCreditsStatus(identity: AuthIdentity | null) {
  return useQuery(
    identity
      ? creditsStatusQuery(identity)
      : { queryKey: accountKeys.creditsStatus(""), queryFn: skipToken, staleTime: ACCOUNT_STALE_MS },
  );
}

/** The DELETE has no reply body, so the status is read again rather than written from the browser. */
export function useOptOutOfCredits() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ identity }: { identity: AuthIdentity }) => optOutOfCredits(identity),
    onSuccess: (_result, { identity }) =>
      client.invalidateQueries({ queryKey: accountKeys.creditsStatus(identity.signer) }),
  });
}

/** Unsigned: the emailed link names the account, which this browser may not be signed in as. */
export function useConfirmEmail() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (confirmation: EmailConfirmation) =>
      confirmEmail(confirmation, { signal: AbortSignal.timeout(SUBSCRIPTION_WRITE_TIMEOUT_MS) }),
    onSuccess: (_result, { address }) =>
      client.invalidateQueries({ queryKey: accountKeys.subscription(address) }),
  });
}

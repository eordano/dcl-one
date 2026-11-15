// GENERATED from library/catalyst-api/notifications by sites/scripts/gen-zod-schemas.mts. Do not edit.
import { z } from "zod";

import type { BroadcastResponse } from "@catalyst-api/notifications/BroadcastResponse";
import type { CommunityOptOutStatus } from "@catalyst-api/notifications/CommunityOptOutStatus";
import type { MarkReadResponse } from "@catalyst-api/notifications/MarkReadResponse";
import type { NotificationItem } from "@catalyst-api/notifications/NotificationItem";
import type { NotificationsListResponse } from "@catalyst-api/notifications/NotificationsListResponse";
import type { Subscription } from "@catalyst-api/notifications/Subscription";

export const BroadcastResponseSchema = z.object({
  ok: z.boolean(),
  broadcastId: z.string(),
  type: z.string(),
  recipients: z.number(),
});

export const CommunityOptOutStatusSchema = z.object({
  scope: z.string(),
  scopeId: z.string(),
  optedOut: z.boolean(),
});

export const MarkReadResponseSchema = z.object({
  updated: z.number(),
});

export const NotificationItemSchema = z.object({
  id: z.string(),
  type: z.string(),
  address: z.string().nullable(),
  timestamp: z.string(),
  read: z.boolean(),
  created_at: z.string(),
  updated_at: z.string(),
  metadata: z.record(z.string(), z.unknown()),
});

export const NotificationsListResponseSchema = z.object({
  notifications: z.array(NotificationItemSchema),
});

export const SubscriptionSchema = z.object({
  address: z.string(),
  email: z.string().nullable().optional(),
  unconfirmedEmail: z.string().optional(),
  details: z.record(z.string(), z.unknown()),
});

type AssignableTo<Sub, Sup> = Sub extends Sup ? true : false;
type Mutual<A, B> = AssignableTo<A, B> extends true ? AssignableTo<B, A> : false;
type Assert<T extends true> = T;

export type _AssertBroadcastResponse = Assert<Mutual<BroadcastResponse, z.infer<typeof BroadcastResponseSchema>>>;
export type _AssertCommunityOptOutStatus = Assert<Mutual<CommunityOptOutStatus, z.infer<typeof CommunityOptOutStatusSchema>>>;
export type _AssertMarkReadResponse = Assert<Mutual<MarkReadResponse, z.infer<typeof MarkReadResponseSchema>>>;
export type _AssertNotificationItem = Assert<Mutual<NotificationItem, z.infer<typeof NotificationItemSchema>>>;
export type _AssertNotificationsListResponse = Assert<Mutual<NotificationsListResponse, z.infer<typeof NotificationsListResponseSchema>>>;
export type _AssertSubscription = Assert<Mutual<Subscription, z.infer<typeof SubscriptionSchema>>>;

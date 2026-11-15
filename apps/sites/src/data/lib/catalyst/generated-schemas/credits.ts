// GENERATED from library/catalyst-api/credits by sites/scripts/gen-zod-schemas.mts. Do not edit.
import { z } from "zod";

import type { AuthorizeCreditOut } from "@catalyst-api/credits/AuthorizeCreditOut";
import type { AuthorizedCreditOut } from "@catalyst-api/credits/AuthorizedCreditOut";
import type { BalanceOut } from "@catalyst-api/credits/BalanceOut";
import type { CartLineOut } from "@catalyst-api/credits/CartLineOut";
import type { CartOut } from "@catalyst-api/credits/CartOut";
import type { CheckoutOut } from "@catalyst-api/credits/CheckoutOut";
import type { CheckoutSessionOut } from "@catalyst-api/credits/CheckoutSessionOut";
import type { CheckoutStartOut } from "@catalyst-api/credits/CheckoutStartOut";
import type { ClaimCreditsResponse } from "@catalyst-api/credits/ClaimCreditsResponse";
import type { CreditFlagReason } from "@catalyst-api/credits/CreditFlagReason";
import type { CreditsData } from "@catalyst-api/credits/CreditsData";
import type { CreditsOrderStatusOut } from "@catalyst-api/credits/CreditsOrderStatusOut";
import type { CreditsProgramProgressResponse } from "@catalyst-api/credits/CreditsProgramProgressResponse";
import type { CreditsTotals } from "@catalyst-api/credits/CreditsTotals";
import type { CurrentSeasonInfo } from "@catalyst-api/credits/CurrentSeasonInfo";
import type { GoalData } from "@catalyst-api/credits/GoalData";
import type { GoalMetadata } from "@catalyst-api/credits/GoalMetadata";
import type { GoalProgressData } from "@catalyst-api/credits/GoalProgressData";
import type { HeldOut } from "@catalyst-api/credits/HeldOut";
import type { HeldPurchaseOut } from "@catalyst-api/credits/HeldPurchaseOut";
import type { ItemQuoteOut } from "@catalyst-api/credits/ItemQuoteOut";
import type { LineOut } from "@catalyst-api/credits/LineOut";
import type { ManaTopupOut } from "@catalyst-api/credits/ManaTopupOut";
import type { ManaTopupQuoteOut } from "@catalyst-api/credits/ManaTopupQuoteOut";
import type { MockPurchaseOut } from "@catalyst-api/credits/MockPurchaseOut";
import type { MockTopupOut } from "@catalyst-api/credits/MockTopupOut";
import type { PackIntentOut } from "@catalyst-api/credits/PackIntentOut";
import type { PackOut } from "@catalyst-api/credits/PackOut";
import type { PriceQuotesOut } from "@catalyst-api/credits/PriceQuotesOut";
import type { ProgressSeason } from "@catalyst-api/credits/ProgressSeason";
import type { ProgressWeek } from "@catalyst-api/credits/ProgressWeek";
import type { PurchaseIntentIn } from "@catalyst-api/credits/PurchaseIntentIn";
import type { SeasonData } from "@catalyst-api/credits/SeasonData";
import type { SeasonsData } from "@catalyst-api/credits/SeasonsData";
import type { SeasonSummary } from "@catalyst-api/credits/SeasonSummary";
import type { UnityCreditPack } from "@catalyst-api/credits/UnityCreditPack";
import type { UnityCreditPacksResponse } from "@catalyst-api/credits/UnityCreditPacksResponse";
import type { UsdCredits } from "@catalyst-api/credits/UsdCredits";
import type { UserCreditItem } from "@catalyst-api/credits/UserCreditItem";
import type { UserCreditsResponse } from "@catalyst-api/credits/UserCreditsResponse";
import type { UserData } from "@catalyst-api/credits/UserData";
import type { Week } from "@catalyst-api/credits/Week";

export const AuthorizedCreditOutSchema = z.object({
  id: z.string(),
  amount: z.string(),
  availableAmount: z.string(),
  expiresAt: z.number(),
  signature: z.string(),
  contract: z.string(),
});

export const LineOutSchema = z.object({
  usdCents: z.number(),
  tradeId: z.string().nullable(),
  contractAddress: z.string().nullable(),
  itemId: z.string().nullable(),
  name: z.string().nullable(),
});

export const AuthorizeCreditOutSchema = z.object({
  credit: AuthorizedCreditOutSchema,
  maxCreditedValue: z.string(),
  usdCents: z.number(),
  oracleRate: z.string(),
  lines: z.array(LineOutSchema).nullable(),
});

export const BalanceOutSchema = z.object({
  address: z.string(),
  available: z.string(),
});

export const CartLineOutSchema = z.object({
  itemId: z.string(),
  collection: z.string(),
  urn: z.string(),
  category: z.string(),
  qty: z.number(),
  unitPriceCredits: z.string(),
});

export const CartOutSchema = z.object({
  address: z.string(),
  items: z.array(CartLineOutSchema),
  totalCredits: z.string(),
});

export const CheckoutOutSchema = z.object({
  id: z.number(),
  address: z.string(),
  totalCredits: z.string(),
  status: z.string(),
});

export const CheckoutSessionOutSchema = z.object({
  orderId: z.string(),
  url: z.string(),
});

export const CheckoutStartOutSchema = z.object({
  id: z.number(),
  status: z.string(),
  replayed: z.boolean(),
});

export const ClaimCreditsResponseSchema = z.object({
  ok: z.boolean(),
  message: z.string(),
  credits_granted: z.number(),
  isBlockedForClaiming: z.boolean(),
});

export const CreditFlagReasonSchema = z.object({
  message: z.string(),
  ip: z.string().nullable(),
  linkedAddresses: z.array(z.string()),
});

export const CreditsDataSchema = z.object({
  available: z.number(),
  expiresIn: z.number(),
  isBlockedForClaiming: z.boolean(),
});

export const CreditsOrderStatusOutSchema = z.object({
  status: z.string(),
  creditsGranted: z.number(),
  newBalance: z.number(),
  error: z.string(),
});

export const GoalMetadataSchema = z.object({
  photosTaken: z.array(z.string()),
});

export const GoalProgressDataSchema = z.object({
  metadata: GoalMetadataSchema,
  totalSteps: z.number(),
  completedSteps: z.number(),
});

export const GoalDataSchema = z.object({
  isCompleted: z.boolean(),
  completedAt: z.number().nullable(),
  id: z.string(),
  title: z.string(),
  description: z.string().nullable(),
  thumbnail: z.string(),
  progress: GoalProgressDataSchema,
  reward: z.number(),
  isClaimed: z.boolean(),
});

export const ProgressSeasonSchema = z.object({
  timeLeft: z.number(),
  startDate: z.string(),
  endDate: z.string(),
  seasonState: z.string(),
});

export const ProgressWeekSchema = z.object({
  timeLeft: z.number(),
});

export const UserDataSchema = z.object({
  hasStartedProgram: z.boolean(),
});

export const CreditsProgramProgressResponseSchema = z.object({
  season: ProgressSeasonSchema,
  currentWeek: ProgressWeekSchema,
  user: UserDataSchema,
  credits: CreditsDataSchema,
  goals: z.array(GoalDataSchema),
});

export const CreditsTotalsSchema = z.object({
  expiring: z.number(),
  nonExpiring: z.number(),
});

export const SeasonDataSchema = z.object({
  id: z.number(),
  name: z.string(),
  startDate: z.string(),
  endDate: z.string(),
  maxMana: z.string(),
  timeLeft: z.number(),
  amountOfWeeks: z.number(),
  state: z.string(),
});

export const WeekSchema = z.object({
  weekNumber: z.number(),
  timeLeft: z.number(),
  startDate: z.string(),
  endDate: z.string(),
  secondsRemaining: z.number(),
});

export const CurrentSeasonInfoSchema = z.object({
  season: SeasonDataSchema,
  week: WeekSchema,
});

export const HeldPurchaseOutSchema = z.object({
  salt: z.string(),
  credits: z.number(),
  releasesAtSeconds: z.number().nullable(),
  contractAddress: z.string().nullable(),
  itemId: z.string().nullable(),
  status: z.string(),
});

export const HeldOutSchema = z.object({
  cents: z.number(),
  credits: z.number(),
  releasesAtSeconds: z.number().nullable(),
  purchases: z.array(HeldPurchaseOutSchema),
});

export const ItemQuoteOutSchema = z.object({
  itemId: z.string(),
  collection: z.string(),
  credits: z.string().nullable(),
});

export const ManaTopupOutSchema = z.object({
  creditsGranted: z.string(),
  available: z.string(),
  txHash: z.string(),
});

export const ManaTopupQuoteOutSchema = z.object({
  credits: z.string(),
  weiSuggested: z.string(),
  manaUsd: z.string(),
});

export const MockPurchaseOutSchema = z.object({
  creditsGranted: z.string(),
  available: z.string(),
  mock: z.boolean(),
});

export const MockTopupOutSchema = z.object({
  creditsGranted: z.string(),
  available: z.string(),
  mock: z.boolean(),
});

export const PackIntentOutSchema = z.object({
  clientSecret: z.string(),
  paymentIntentId: z.string(),
});

export const PackOutSchema = z.object({
  sku: z.string(),
  title: z.string(),
  credits: z.string(),
  priceCents: z.number(),
  currency: z.string(),
  sortOrder: z.number(),
});

export const PriceQuotesOutSchema = z.object({
  items: z.array(ItemQuoteOutSchema),
  amounts: z.array(z.string().nullable()),
});

export const PurchaseIntentInSchema = z.object({
  buyer: z.string(),
  items: z.string(),
  totalCredits: z.string(),
  currency: z.string(),
  nonce: z.string(),
  expiresAt: z.number(),
});

export const SeasonSummarySchema = z.object({
  id: z.number(),
  name: z.string(),
  startDate: z.string(),
  endDate: z.string(),
  maxMana: z.string(),
});

export const SeasonsDataSchema = z.object({
  lastSeason: SeasonSummarySchema.nullable(),
  currentSeason: CurrentSeasonInfoSchema.nullable(),
  nextSeason: SeasonSummarySchema.nullable(),
});

export const UnityCreditPackSchema = z.object({
  id: z.string(),
  usd: z.number(),
  credits: z.number(),
  recommended: z.boolean(),
  order: z.number(),
});

export const UnityCreditPacksResponseSchema = z.object({
  packs: z.array(UnityCreditPackSchema),
});

export const UsdCreditsSchema = z.object({
  balanceCents: z.coerce.bigint(),
  credits: z.number(),
  held: HeldOutSchema.optional(),
});

export const UserCreditItemSchema = z.object({
  id: z.string(),
  userAddress: z.string(),
  amount: z.string(),
  availableAmount: z.string(),
  status: z.string(),
  contract: z.string(),
  timestamp: z.string(),
  signature: z.string(),
  seasonId: z.number(),
  goalId: z.string(),
  weekId: z.number(),
  claimedAt: z.string(),
  expiresAt: z.string(),
  creditSource: z.string(),
});

export const UserCreditsResponseSchema = z.object({
  message: z.string().optional(),
  reason: CreditFlagReasonSchema.optional(),
  isBlockedForClaiming: z.boolean().optional(),
  credits: z.array(UserCreditItemSchema),
  totalCredits: z.number(),
  totals: CreditsTotalsSchema,
  usd: UsdCreditsSchema,
});

type AssignableTo<Sub, Sup> = Sub extends Sup ? true : false;
type Mutual<A, B> = AssignableTo<A, B> extends true ? AssignableTo<B, A> : false;
type Assert<T extends true> = T;

export type _AssertAuthorizeCreditOut = Assert<Mutual<AuthorizeCreditOut, z.infer<typeof AuthorizeCreditOutSchema>>>;
export type _AssertAuthorizedCreditOut = Assert<Mutual<AuthorizedCreditOut, z.infer<typeof AuthorizedCreditOutSchema>>>;
export type _AssertBalanceOut = Assert<Mutual<BalanceOut, z.infer<typeof BalanceOutSchema>>>;
export type _AssertCartLineOut = Assert<Mutual<CartLineOut, z.infer<typeof CartLineOutSchema>>>;
export type _AssertCartOut = Assert<Mutual<CartOut, z.infer<typeof CartOutSchema>>>;
export type _AssertCheckoutOut = Assert<Mutual<CheckoutOut, z.infer<typeof CheckoutOutSchema>>>;
export type _AssertCheckoutSessionOut = Assert<Mutual<CheckoutSessionOut, z.infer<typeof CheckoutSessionOutSchema>>>;
export type _AssertCheckoutStartOut = Assert<Mutual<CheckoutStartOut, z.infer<typeof CheckoutStartOutSchema>>>;
export type _AssertClaimCreditsResponse = Assert<Mutual<ClaimCreditsResponse, z.infer<typeof ClaimCreditsResponseSchema>>>;
export type _AssertCreditFlagReason = Assert<Mutual<CreditFlagReason, z.infer<typeof CreditFlagReasonSchema>>>;
export type _AssertCreditsData = Assert<Mutual<CreditsData, z.infer<typeof CreditsDataSchema>>>;
export type _AssertCreditsOrderStatusOut = Assert<Mutual<CreditsOrderStatusOut, z.infer<typeof CreditsOrderStatusOutSchema>>>;
export type _AssertCreditsProgramProgressResponse = Assert<Mutual<CreditsProgramProgressResponse, z.infer<typeof CreditsProgramProgressResponseSchema>>>;
export type _AssertCreditsTotals = Assert<Mutual<CreditsTotals, z.infer<typeof CreditsTotalsSchema>>>;
export type _AssertCurrentSeasonInfo = Assert<Mutual<CurrentSeasonInfo, z.infer<typeof CurrentSeasonInfoSchema>>>;
export type _AssertGoalData = Assert<Mutual<GoalData, z.infer<typeof GoalDataSchema>>>;
export type _AssertGoalMetadata = Assert<Mutual<GoalMetadata, z.infer<typeof GoalMetadataSchema>>>;
export type _AssertGoalProgressData = Assert<Mutual<GoalProgressData, z.infer<typeof GoalProgressDataSchema>>>;
export type _AssertHeldOut = Assert<Mutual<HeldOut, z.infer<typeof HeldOutSchema>>>;
export type _AssertHeldPurchaseOut = Assert<Mutual<HeldPurchaseOut, z.infer<typeof HeldPurchaseOutSchema>>>;
export type _AssertItemQuoteOut = Assert<Mutual<ItemQuoteOut, z.infer<typeof ItemQuoteOutSchema>>>;
export type _AssertLineOut = Assert<Mutual<LineOut, z.infer<typeof LineOutSchema>>>;
export type _AssertManaTopupOut = Assert<Mutual<ManaTopupOut, z.infer<typeof ManaTopupOutSchema>>>;
export type _AssertManaTopupQuoteOut = Assert<Mutual<ManaTopupQuoteOut, z.infer<typeof ManaTopupQuoteOutSchema>>>;
export type _AssertMockPurchaseOut = Assert<Mutual<MockPurchaseOut, z.infer<typeof MockPurchaseOutSchema>>>;
export type _AssertMockTopupOut = Assert<Mutual<MockTopupOut, z.infer<typeof MockTopupOutSchema>>>;
export type _AssertPackIntentOut = Assert<Mutual<PackIntentOut, z.infer<typeof PackIntentOutSchema>>>;
export type _AssertPackOut = Assert<Mutual<PackOut, z.infer<typeof PackOutSchema>>>;
export type _AssertPriceQuotesOut = Assert<Mutual<PriceQuotesOut, z.infer<typeof PriceQuotesOutSchema>>>;
export type _AssertProgressSeason = Assert<Mutual<ProgressSeason, z.infer<typeof ProgressSeasonSchema>>>;
export type _AssertProgressWeek = Assert<Mutual<ProgressWeek, z.infer<typeof ProgressWeekSchema>>>;
export type _AssertPurchaseIntentIn = Assert<Mutual<PurchaseIntentIn, z.infer<typeof PurchaseIntentInSchema>>>;
export type _AssertSeasonData = Assert<Mutual<SeasonData, z.infer<typeof SeasonDataSchema>>>;
export type _AssertSeasonsData = Assert<Mutual<SeasonsData, z.infer<typeof SeasonsDataSchema>>>;
export type _AssertSeasonSummary = Assert<Mutual<SeasonSummary, z.infer<typeof SeasonSummarySchema>>>;
export type _AssertUnityCreditPack = Assert<Mutual<UnityCreditPack, z.infer<typeof UnityCreditPackSchema>>>;
export type _AssertUnityCreditPacksResponse = Assert<Mutual<UnityCreditPacksResponse, z.infer<typeof UnityCreditPacksResponseSchema>>>;
export type _AssertUsdCredits = Assert<Mutual<UsdCredits, z.infer<typeof UsdCreditsSchema>>>;
export type _AssertUserCreditItem = Assert<Mutual<UserCreditItem, z.infer<typeof UserCreditItemSchema>>>;
export type _AssertUserCreditsResponse = Assert<Mutual<UserCreditsResponse, z.infer<typeof UserCreditsResponseSchema>>>;
export type _AssertUserData = Assert<Mutual<UserData, z.infer<typeof UserDataSchema>>>;
export type _AssertWeek = Assert<Mutual<Week, z.infer<typeof WeekSchema>>>;

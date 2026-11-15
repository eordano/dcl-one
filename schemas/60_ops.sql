-- Catalyst v2: the operator plane and the remaining services: credits (the
-- Marketplace Credits ledger, Stripe purchases, carts, checkouts and
-- authorizations) and ops (the ONE operator audit).
--
-- Designed from the workload, not copied: the old crates' databases
-- (`credits`, the server's own) collapse into two schemas of `catalyst`.
-- Sources: catalyrst-credits/migrations 0001..0021, the old server's
-- admin_audit (catalyrst-server/src/admin/audit.rs).
--
-- Conventions, as in the other files: wallet addresses are lowercase 0x + 40
-- hex text where this node writes them (CHECKed); every timestamp is
-- `timestamp without time zone` holding UTC (the pool pins TimeZone=UTC; the
-- old tables were timestamptz); the current time is a bind parameter in
-- every due, expiry, window and purge predicate, never in an index
-- predicate; credits amounts are numeric (never a float in SQL or Rust), wei
-- is numeric(78,0). No FK crosses a schema: `credits.ledger.admin_audit_id`
-- names an `ops.admin_audit` row and `credits.fulfillment_outbox.external_ref`
-- an economy broker purchase, reconciled on read.
--
-- Authority: the credits ledger, checkouts, authorizations and the operator
-- audit are LOCAL authority behind OPS_WRITES_ENABLED (default false; 503
-- envelope like src/social/writes.rs). The former `telemetry` schema (the
-- Sentry/Segment sink and its console) is retired: a database stamped before
-- the removal drops it through schemas/releases/telemetry-retired.sql;
-- client telemetry goes to Segment.
--
-- Domain agents append ONLY inside the `-- ==== section: ... ====` blocks at
-- the end, after re-reading this file.

CREATE SCHEMA credits;
CREATE SCHEMA ops;

-- ══════════════════════════════════════════════════════════════════════════
-- credits
-- ══════════════════════════════════════════════════════════════════════════

-- One row per wallet that touched the program: the old user_program
-- (enrolment) and user_credits (balance) rows shared the key and always
-- moved together, so they are one row. `available` is the TOTAL spendable
-- balance and `earned_available` the slice that came from claims; both are
-- projections of `ledger` (signed sum per bucket) written in the SAME
-- transaction as the ledger row, under `SELECT ... FOR UPDATE` on this row
-- (the wallet lock every spend, grant, refund, revoke and authorization
-- takes); the reconciler asserts the equality and records any drift as an
-- explicit adjustment ledger row rather than tolerating it. Earned credits
-- no longer expire (old migration 0017), so no expiry column.
CREATE TABLE credits.wallets (
    address text NOT NULL,
    has_started_program boolean NOT NULL DEFAULT true,
    available numeric NOT NULL DEFAULT 0,
    earned_available numeric NOT NULL DEFAULT 0,
    is_blocked_for_claiming boolean NOT NULL DEFAULT false,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT wallets_pkey PRIMARY KEY (address),
    CONSTRAINT wallets_address_check CHECK (address ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT wallets_available_check CHECK (available >= 0),
    CONSTRAINT wallets_earned_bound_check CHECK (earned_available >= 0 AND earned_available <= available)
);

-- The multi-accounting flag notified on the `credits.wallet_flags` channel
-- (`{action: "flag", addresses, source, ip}`, the shape the events-notifier
-- posted to the credits server's `/flag`) or posted through the admin's
-- `POST /credits/admin/flags` (the same body): one row per
-- flagged wallet with the source that raised it, the IP it reported and the
-- wallets it was linked with. The claim reads it as part of its block
-- (shop/src/credits/store.rs `is_blocked_for_claiming`): a flagged wallet
-- claims no credits and keeps spending the ones it holds. Rows never expire;
-- the admin's unflag and the operator's unblock delete them.
CREATE TABLE credits.wallet_flags (
    address text NOT NULL,
    source text NOT NULL,
    ip text,
    linked_addresses text[] NOT NULL DEFAULT '{}',
    flagged_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT wallet_flags_pkey PRIMARY KEY (address),
    CONSTRAINT wallet_flags_address_check CHECK (address ~ '^0x[0-9a-f]{40}$')
);

-- The append-only money fact: every movement of a wallet's balance, tagged
-- with the bucket it moved. credit = grant, claim, purchase, refund; debit =
-- spend, consume, expire (the sign is the kind's, `amount` is never
-- negative). `tx_ref` is the cause (a checkout id, a Stripe event id, an
-- authorization id, an operator reason key); `admin_audit_id` names the
-- ops.admin_audit row of an operator grant/revoke/refund (no FK across the
-- schema; the audit row is written first in the same transaction).
CREATE TABLE credits.ledger (
    id bigint GENERATED ALWAYS AS IDENTITY,
    address text NOT NULL,
    kind text NOT NULL,
    bucket text NOT NULL DEFAULT 'paid',
    amount numeric NOT NULL,
    tx_ref text,
    captcha_ok boolean NOT NULL DEFAULT false,
    admin_audit_id bigint,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT ledger_pkey PRIMARY KEY (id),
    CONSTRAINT ledger_address_check CHECK (address ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT ledger_kind_check CHECK (kind IN ('grant', 'claim', 'expire', 'consume', 'spend', 'purchase', 'refund')),
    CONSTRAINT ledger_bucket_check CHECK (bucket IN ('earned', 'paid')),
    CONSTRAINT ledger_amount_check CHECK (amount >= 0)
);

-- The wallet's ledger page (/admin/credits/ledger?address= ORDER BY id DESC
-- LIMIT/OFFSET) and the reconciler's per-wallet signed sums.
CREATE INDEX ledger_address_id_idx ON credits.ledger (address, id DESC);
-- The refund window: SUM(spend) - SUM(refund) of one tx_ref for one wallet
-- (refund_in_tx clamps a refund to what that cause actually debited).
CREATE INDEX ledger_tx_ref_idx ON credits.ledger (tx_ref, address) WHERE tx_ref IS NOT NULL;

-- Idempotent money writes: the three old tables (grant / spend / refund
-- idempotency) had the same columns and the same rule -- the first write
-- for a key records its result inside the write's transaction, a replay
-- returns that result without moving money -- so they are one relation
-- keyed by (scope, key). `amount` is what was REQUESTED (the replay guard
-- compares against it), `applied` what actually moved (a clamped refund),
-- `available` the balance the original answer carried.
CREATE TABLE credits.idempotency (
    scope text NOT NULL,
    idempotency_key text NOT NULL,
    address text NOT NULL,
    amount numeric NOT NULL,
    applied numeric,
    available numeric NOT NULL,
    tx_ref text,
    actor text,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT idempotency_pkey PRIMARY KEY (scope, idempotency_key),
    CONSTRAINT idempotency_scope_check CHECK (scope IN ('grant', 'spend', 'refund'))
);

-- Buyable Credit packs; `credits` is the numeric Credits granted on
-- purchase, `price_cents` the fiat charge in minor units. Product
-- configuration: the four tiers at the 0.10 USD/Credit peg are seeded here
-- (the old migration 0014) and edited by /admin/credits/packs.
CREATE TABLE credits.packs (
    sku text NOT NULL,
    title text NOT NULL,
    credits numeric NOT NULL,
    price_cents bigint NOT NULL,
    currency text NOT NULL,
    active boolean NOT NULL DEFAULT true,
    sort_order integer NOT NULL DEFAULT 0,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT packs_pkey PRIMARY KEY (sku),
    CONSTRAINT packs_credits_check CHECK (credits > 0),
    CONSTRAINT packs_price_check CHECK (price_cents >= 0)
);
INSERT INTO credits.packs (sku, title, credits, price_cents, currency, active, sort_order) VALUES
    ('credits_usd_5',  'Starter',  50,   500, 'usd', true, 1),
    ('credits_usd_15', 'Plus',    150,  1500, 'usd', true, 2),
    ('credits_usd_25', 'Pro',     250,  2500, 'usd', true, 3),
    ('credits_usd_50', 'Max',     500,  5000, 'usd', true, 4);

-- Stripe delivery de-duplication: one row per event id, `processed_at`
-- stamped only after every side effect committed, so a delivery recorded
-- but not processed is re-driven on Stripe's retry (the effects are
-- idempotent through `idempotency`) and a crash between the two never drops
-- a paid customer's Credits. The webhook signature (Stripe-Signature, HMAC
-- over t.body, 5 minute tolerance) is verified before this row exists.
CREATE TABLE credits.stripe_events (
    event_id text NOT NULL,
    type text NOT NULL,
    payload jsonb,
    received_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    processed_at timestamp without time zone,
    CONSTRAINT stripe_events_pkey PRIMARY KEY (event_id)
);

-- One row per pack purchase or Credits order: a PaymentIntent (`method` =
-- 'card', the intent flow), a Checkout Session (`order_id` +
-- `stripe_checkout_session`, the explorer's order flow) or a MANA top-up
-- (`method` = 'mana', no Stripe columns). `credits` and `amount_cents` are
-- captured at creation and the webhook grants from THIS row, never from the
-- event's metadata. `refunded_cents` is the cumulative fiat clawed back
-- (charge.refunded carries a cumulative figure; each event refunds the
-- delta), `revoked_credits` the Credits removed for it, structurally bounded
-- by the grant.
CREATE TABLE credits.purchases (
    id bigint GENERATED ALWAYS AS IDENTITY,
    address text NOT NULL,
    sku text NOT NULL,
    credits numeric NOT NULL,
    amount_cents bigint NOT NULL,
    -- Packs this checkout bought; credits and amount_cents already hold the
    -- totals, so it moves no money (the reports tell 3 x pack from 1 x pack).
    quantity integer NOT NULL DEFAULT 1,
    -- The surface that opened a Checkout Session (NULL: unknown or another flow);
    -- it also chose the page Stripe returned the buyer to.
    source text,
    currency text NOT NULL,
    method text NOT NULL DEFAULT 'card',
    status text NOT NULL DEFAULT 'pending',
    order_id text,
    stripe_payment_intent text,
    stripe_checkout_session text,
    stripe_event_id text,
    refunded_cents bigint NOT NULL DEFAULT 0,
    revoked_credits numeric NOT NULL DEFAULT 0,
    failure_reason text,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT purchases_pkey PRIMARY KEY (id),
    CONSTRAINT purchases_address_check CHECK (address ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT purchases_status_check
        CHECK (status IN ('pending', 'paid', 'refunded', 'disputed', 'failed', 'abandoned')),
    CONSTRAINT purchases_refunded_bound_check CHECK (refunded_cents >= 0 AND refunded_cents <= amount_cents),
    CONSTRAINT purchases_revoked_bound_check CHECK (revoked_credits >= 0 AND revoked_credits <= credits),
    CONSTRAINT purchases_quantity_check CHECK (quantity >= 1),
    CONSTRAINT purchases_source_check CHECK (source IN ('website', 'client', 'builder'))
);

-- The webhook's lookups: one purchase per PaymentIntent / Checkout Session /
-- order id (NULL for the flows that have none).
CREATE UNIQUE INDEX purchases_payment_intent_uidx ON credits.purchases (stripe_payment_intent)
    WHERE stripe_payment_intent IS NOT NULL;
CREATE UNIQUE INDEX purchases_checkout_session_uidx ON credits.purchases (stripe_checkout_session)
    WHERE stripe_checkout_session IS NOT NULL;
CREATE UNIQUE INDEX purchases_order_id_uidx ON credits.purchases (order_id) WHERE order_id IS NOT NULL;
-- The reconciler's paid-vs-granted sums per wallet and /admin/credits/purchases?address=.
CREATE INDEX purchases_address_idx ON credits.purchases (address, id DESC);

-- Stripe reversals per PaymentIntent, recorded whether or not its purchase is
-- credited yet: Stripe does not order deliveries, so a `charge.refunded` or
-- `charge.dispute.*` can arrive before the settlement that credits the
-- purchase. The webhook writes it under the PaymentIntent's advisory lock and
-- the grant applies it in its own transaction (same lock), bounded by the
-- purchase. `refunded_cents` is the highest cumulative `amount_refunded`
-- seen; `disputed` latches on the first dispute. One row per reversed
-- PaymentIntent (refunds and disputes are rare), read by primary key once
-- per grant; no foreign key, since the purchase may not be linked yet.
CREATE TABLE credits.payment_reversals (
    payment_intent text NOT NULL,
    refunded_cents bigint NOT NULL DEFAULT 0,
    disputed boolean NOT NULL DEFAULT false,
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT payment_reversals_pkey PRIMARY KEY (payment_intent),
    CONSTRAINT payment_reversals_refunded_check CHECK (refunded_cents >= 0)
);

-- One open cart per wallet; the address is the natural key.
CREATE TABLE credits.carts (
    id bigint GENERATED ALWAYS AS IDENTITY,
    address text NOT NULL,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT carts_pkey PRIMARY KEY (id),
    CONSTRAINT carts_address_key UNIQUE (address),
    CONSTRAINT carts_address_check CHECK (address ~ '^0x[0-9a-f]{40}$')
);

-- A cart line, identified by (collection, item_id): the item id alone is a
-- per-collection blockchain index (nearly every collection has an item "0"),
-- which the old UNIQUE (cart_id, item_id) got wrong. `unit_price_credits`
-- is the server price captured at add time and re-computed at checkout; the
-- client's price is never read.
CREATE TABLE credits.cart_items (
    id bigint GENERATED ALWAYS AS IDENTITY,
    cart_id bigint NOT NULL,
    collection text NOT NULL,
    item_id text NOT NULL,
    urn text NOT NULL,
    category text NOT NULL,
    qty integer NOT NULL DEFAULT 1,
    unit_price_credits numeric NOT NULL,
    added_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT cart_items_pkey PRIMARY KEY (id),
    CONSTRAINT cart_items_cart_fkey FOREIGN KEY (cart_id) REFERENCES credits.carts (id) ON DELETE CASCADE,
    CONSTRAINT cart_items_line_key UNIQUE (cart_id, collection, item_id),
    CONSTRAINT cart_items_collection_check CHECK (collection ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT cart_items_qty_check CHECK (qty > 0),
    CONSTRAINT cart_items_price_check CHECK (unit_price_credits >= 0)
);

-- One row per checkout attempt, keyed by the client's Idempotency-Key so a
-- retried POST /checkout re-finds its row instead of debiting twice.
-- `total_credits` is the server-priced total debited ONCE (reject-if-short,
-- never a clamp) as the ledger spend row `ledger_id`; the outbox below then
-- fulfils each unit asynchronously. 'reversing' freezes a checkout whose
-- line failed terminally while the undelivered portion is refunded; it
-- resolves to 'reversed' (nothing delivered) or 'failed' (partial).
CREATE TABLE credits.checkouts (
    id bigint GENERATED ALWAYS AS IDENTITY,
    idempotency_key text NOT NULL,
    address text NOT NULL,
    total_credits numeric NOT NULL DEFAULT 0,
    status text NOT NULL DEFAULT 'reserving',
    ledger_id bigint,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT checkouts_pkey PRIMARY KEY (id),
    CONSTRAINT checkouts_idempotency_key_key UNIQUE (idempotency_key),
    CONSTRAINT checkouts_address_check CHECK (address ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT checkouts_status_check
        CHECK (status IN ('reserving', 'debited', 'fulfilling', 'fulfilled', 'failed', 'reversing', 'reversed')),
    CONSTRAINT checkouts_total_check CHECK (total_credits >= 0)
);

-- /admin/credits/checkouts?address= and the wallet's own checkout history, newest first.
CREATE INDEX checkouts_address_id_idx ON credits.checkouts (address, id DESC);

-- The fulfilment outbox: one row per charged UNIT (a line of qty N is N
-- rows, one broker buy each), drained by the `credits-fulfillment` job.
-- `mode` selects the economy broker path; `token_id` + `basis_wei` pin a
-- secondary listing (MarketplaceV2 binds both on-chain, so fulfilment can
-- never pay a different amount than the buyer was charged for), `trade_id`
-- pins a v3 signed trade (re-fetched from shop.trades and checked
-- still open at fulfilment); `external_ref` is the broker's purchase
-- reference once confirmed. SUM(unit_price_credits) over a checkout's rows
-- equals its total_credits by construction.
CREATE TABLE credits.fulfillment_outbox (
    id bigint GENERATED ALWAYS AS IDENTITY,
    checkout_id bigint NOT NULL,
    item_id text NOT NULL,
    collection text NOT NULL,
    urn text NOT NULL,
    token_id text,
    trade_id text,
    basis_wei numeric(78,0),
    unit_price_credits numeric NOT NULL,
    mode text NOT NULL,
    status text NOT NULL DEFAULT 'pending',
    attempts integer NOT NULL DEFAULT 0,
    last_error text,
    external_ref text,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT fulfillment_outbox_pkey PRIMARY KEY (id),
    CONSTRAINT fulfillment_outbox_checkout_fkey FOREIGN KEY (checkout_id) REFERENCES credits.checkouts (id),
    CONSTRAINT fulfillment_outbox_collection_check CHECK (collection ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT fulfillment_outbox_mode_check CHECK (mode IN ('primary', 'secondary', 'trade')),
    CONSTRAINT fulfillment_outbox_status_check CHECK (status IN ('pending', 'sent', 'confirmed', 'failed')),
    CONSTRAINT fulfillment_outbox_attempts_check CHECK (attempts >= 0)
);

-- The worker's due set: pending rows (attempts < $max, joined to a
-- 'fulfilling' checkout by PK) claimed one at a time FOR UPDATE SKIP LOCKED
-- in id order; the index holds only the waiting rows.
CREATE INDEX fulfillment_outbox_pending_idx ON credits.fulfillment_outbox (id) WHERE status = 'pending';
-- A checkout's lines (the checkout view, compensation, the confirmed sum the reconciler checks).
CREATE INDEX fulfillment_outbox_checkout_idx ON credits.fulfillment_outbox (checkout_id, id);
-- The escrow grant lookup by broker reference (/admin/grants/{escrow_ref}/*, the reconciler's escrow pass).
CREATE INDEX fulfillment_outbox_external_ref_idx ON credits.fulfillment_outbox (external_ref)
    WHERE external_ref IS NOT NULL;

-- One ephemeral credit (src/shop/credits/{authorize,batch,settlement}.rs;
-- docs/shop/CREDIT-HASH.md, CONTRACTS.md): a settlement group of up to 30
-- lines (`authorization_lines`) pledged as ONE signed credit whose salt is
-- `id`. `usd_cents` is the charged total (each line's quote rounded up to a
-- whole credit), `quoted_cents` the quoted total the signed value is sized
-- from (the oracle rate `oracle_rate` at `oracle_decimals`, the buffer and
-- the cap headroom), `amount_wei` the signed value, `contract` the
-- CreditsManager and `chain_id` its chain, `credit_hash` the contract's
-- identity of the credit (the revocation key; the indexed `_creditId` of
-- CreditUsed), `expires_at` the credit's expiry in the row clock (the
-- signed `expiresAt` is its unix SECONDS). A pledge counts against the
-- balance while it is `authorized` or `retired` (the buyer cancelled it,
-- which is not proof the signed credit became unspendable); the sweep moves
-- both to `released`/`expired` only once the CreditUsed index has read the
-- chain past the expiry (`usage_index_cursor.folded_block_time`), and the
-- settlement pass moves any of them to `consumed` when a CreditUsed log
-- names the salt, however late. `submitted_tx_hash` is the transaction the
-- client reported submitting the credit in: a fact about an attempt, never
-- a verdict (the outcome is `status` and `usage_settlements`).
CREATE TABLE credits.authorizations (
    id text NOT NULL,
    address text NOT NULL,
    usd_cents bigint NOT NULL,
    amount_wei numeric(78,0) NOT NULL,
    trade_id text,
    contract_address text,
    item_id text,
    source text,
    status text NOT NULL DEFAULT 'authorized',
    idempotency_key text,
    expires_at timestamp without time zone NOT NULL,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    chain_id bigint,
    contract text,
    credit_hash text,
    quoted_cents bigint,
    oracle_rate numeric(78,0),
    oracle_decimals smallint,
    submitted_tx_hash text,
    submitted_at timestamp without time zone,
    retired_at timestamp without time zone,
    consumed_at timestamp without time zone,
    -- A request that names its purchase (operationId) keeps what it asked for, so the same
    -- operation with other lines is refused (src/credits/handlers/authorize.rs).
    request_digest text,
    CONSTRAINT authorizations_pkey PRIMARY KEY (id),
    CONSTRAINT authorizations_address_check CHECK (address ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT authorizations_status_check CHECK (status IN ('authorized', 'retired', 'released', 'consumed', 'expired')),
    CONSTRAINT authorizations_usd_check CHECK (usd_cents > 0),
    CONSTRAINT authorizations_credit_hash_check CHECK (credit_hash IS NULL OR credit_hash ~ '^0x[0-9a-f]{64}$'),
    CONSTRAINT authorizations_submitted_tx_check CHECK (submitted_tx_hash IS NULL OR submitted_tx_hash ~ '^0x[0-9a-f]{64}$')
);

-- The idempotent reserve (the client's key, one authorization per key).
CREATE UNIQUE INDEX authorizations_idempotency_key_uidx ON credits.authorizations (idempotency_key)
    WHERE idempotency_key IS NOT NULL;
-- The outstanding budget: SUM(usd_cents) WHERE address = $1 AND status IN
-- ('authorized', 'retired'), the wallet's own list and the purchases
-- history (address, newest first).
CREATE INDEX authorizations_address_status_expiry_idx ON credits.authorizations (address, status, expires_at);
-- The expiry sweep (status IN ('authorized', 'retired') AND expires_at <= $bound), run by the fulfilment tick.
CREATE INDEX authorizations_expiry_idx ON credits.authorizations (expires_at) WHERE status IN ('authorized', 'retired');
-- The purchases history: the buyer's groups newest first, keyset on (created_at, id).
CREATE INDEX authorizations_address_created_idx ON credits.authorizations (address, created_at DESC, id);

-- The lines of one credit, in request order: what each line bought (the
-- payment rule's derivation: `sale_kind`/`asset_kind` are what
-- shop::policy derived from the mirrors and the decoded order, never a
-- client flag), what it was quoted and charged. SUM(charged_cents) over a
-- credit's lines equals the credit's usd_cents by construction (the
-- reconciler checks it). Lines share their credit's lifecycle: a group is
-- settled, retired or expired as one.
CREATE TABLE credits.authorization_lines (
    authorization_id text NOT NULL,
    position integer NOT NULL,
    quoted_cents bigint NOT NULL,
    charged_cents bigint NOT NULL,
    sale_kind text NOT NULL,
    asset_kind text NOT NULL,
    asset_chain_id bigint NOT NULL,
    asset_contract text NOT NULL,
    asset_item_id numeric(78,0),
    asset_token_id numeric(78,0),
    trade_id text,
    contract_address text,
    item_id text,
    registered_name text,
    CONSTRAINT authorization_lines_pkey PRIMARY KEY (authorization_id, position),
    CONSTRAINT authorization_lines_authorization_fkey FOREIGN KEY (authorization_id)
        REFERENCES credits.authorizations (id) ON DELETE CASCADE,
    CONSTRAINT authorization_lines_quoted_check CHECK (quoted_cents > 0),
    CONSTRAINT authorization_lines_charged_check CHECK (charged_cents >= quoted_cents AND charged_cents % 10 = 0),
    CONSTRAINT authorization_lines_sale_kind_check CHECK (sale_kind IN ('primary_mint', 'primary_name_registration')),
    CONSTRAINT authorization_lines_asset_kind_check CHECK (asset_kind IN ('wearable', 'emote', 'name')),
    CONSTRAINT authorization_lines_asset_contract_check CHECK (asset_contract ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT authorization_lines_position_check CHECK (position >= 0 AND position < 30)
);

-- One ledger effect per CreditUsed log: the exact-once key of settlement
-- (src/shop/credits/settlement.rs). The first log of a salt settles the
-- purchase (`kind = 'purchase'`: the credit's charged usd_cents, the debit
-- the group was quoted); any later log of the same salt is a remainder the
-- buyer spent directly (`kind = 'remainder'`: `value_wei` at the credit's
-- oracle rate, rounded up to whole credits), charged like the purchase so
-- no signed value is ever free (docs/shop/CONTRACTS.md, "No spendable
-- remainder"). `tx_ref` is the ledger spend's reference
-- (`credit-used:<salt>:<block>:<log>`); a replay finds this row and moves
-- nothing. A debit the balance could not cover has no row and is retried by
-- the reconciler.
CREATE TABLE credits.usage_settlements (
    chain_id bigint NOT NULL,
    block_number bigint NOT NULL,
    log_index integer NOT NULL,
    authorization_id text NOT NULL,
    address text NOT NULL,
    kind text NOT NULL,
    value_wei numeric(78,0) NOT NULL,
    debited_credits numeric NOT NULL,
    tx_ref text NOT NULL,
    settled_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT usage_settlements_pkey PRIMARY KEY (chain_id, block_number, log_index),
    CONSTRAINT usage_settlements_address_check CHECK (address ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT usage_settlements_kind_check CHECK (kind IN ('purchase', 'remainder')),
    CONSTRAINT usage_settlements_debited_check CHECK (debited_credits >= 0)
);

-- A credit's settlements (the purchases history's txHash and settled MANA, the reconciler's per-credit sum).
CREATE INDEX usage_settlements_authorization_idx ON credits.usage_settlements (authorization_id, block_number, log_index);

-- The revocation outbox: a credit hash to `revokeCredits` on the manager
-- (docs/shop/CONTRACTS.md: as soon as a credit's first CreditUsed is
-- indexed, and on a refund), drained by the `credits-revocations` job with
-- the `CREDITS_REVOKER_KEY_FILE` signer when one is configured. The row is
-- written in the transaction that decides the revocation; the broadcast
-- runs outside it, `tx_hash` is stored before `eth_sendRawTransaction`
-- (a kill between the two leaves a pollable row; re-sending revokeCredits
-- for an already revoked hash is a no-op on chain).
CREATE TABLE credits.revocations (
    credit_hash text NOT NULL,
    authorization_id text NOT NULL,
    chain_id bigint NOT NULL,
    contract text NOT NULL,
    reason text NOT NULL,
    status text NOT NULL DEFAULT 'pending',
    tx_hash text,
    attempts integer NOT NULL DEFAULT 0,
    last_error text,
    created_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT revocations_pkey PRIMARY KEY (credit_hash),
    CONSTRAINT revocations_credit_hash_check CHECK (credit_hash ~ '^0x[0-9a-f]{64}$'),
    CONSTRAINT revocations_contract_check CHECK (contract ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT revocations_status_check CHECK (status IN ('pending', 'sent', 'confirmed', 'failed')),
    CONSTRAINT revocations_attempts_check CHECK (attempts >= 0)
);

-- The job's due set: unfinished revocations, oldest first.
CREATE INDEX revocations_due_idx ON credits.revocations (created_at) WHERE status IN ('pending', 'sent');

-- Slider captchas: GET reuses a live answer and refreshes its two-hour TTL
-- and generated_at; POST consumes the challenge, requires a solve after one
-- second and a strict error below three. Three failures block claiming for
-- 24 hours after the last failure. A configured native hCaptcha provider
-- adds a server-side token check outside the SQL transactions.
CREATE TABLE credits.captcha_challenges (
    id bigint GENERATED ALWAYS AS IDENTITY,
    address text NOT NULL,
    answer_x numeric NOT NULL,
    generated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    expires_at timestamp without time zone NOT NULL,
    consumed_at timestamp without time zone,
    CONSTRAINT captcha_challenges_pkey PRIMARY KEY (id),
    CONSTRAINT captcha_challenges_address_check CHECK (address ~ '^0x[0-9a-f]{40}$')
);

-- The wallet's open challenge (WHERE address = $1 AND consumed_at IS NULL
-- ORDER BY id DESC LIMIT 1) and the invalidating UPDATE over the same set.
CREATE INDEX captcha_challenges_open_idx ON credits.captcha_challenges (address, id DESC) WHERE consumed_at IS NULL;
-- The purge (expires_at < $now - 1 day, batched), part of the ops retention job.
CREATE INDEX captcha_challenges_expires_idx ON credits.captcha_challenges (expires_at);

-- ══════════════════════════════════════════════════════════════════════════
-- ops
-- ══════════════════════════════════════════════════════════════════════════

-- The ONE operator audit: every mutation an operator makes through the
-- console or the admin bearers (sync pause/resume/force, cache flush,
-- credits grant/revoke/refund/block/packs, rpc method and upstream edits,
-- ...), append-only: the application only ever INSERTs, in the same transaction
-- as the write it records where the write is SQL. `actor` is the
-- authenticated identity (a console wallet, `token:<name>` for a bearer,
-- `loopback`); `domain` + `action` the verb (`content.sync.pause`,
-- `credits.grant`), `target` the row or key acted on (a wallet, a checkout
-- id, an rpc method), `detail` the request as received (never a secret),
-- `result` the outcome. The old per-service audit tables (telemetry
-- admin_audit, credits admin_audit, the server's content admin_audit) were
-- the same fact under three shapes; the CHECK keeps the `telemetry` domain
-- for the rows the retired sink console wrote.
CREATE TABLE ops.admin_audit (
    id bigint GENERATED ALWAYS AS IDENTITY,
    at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    actor text NOT NULL,
    domain text NOT NULL,
    action text NOT NULL,
    target text,
    detail jsonb NOT NULL DEFAULT '{}'::jsonb,
    result text NOT NULL DEFAULT 'ok',
    CONSTRAINT admin_audit_pkey PRIMARY KEY (id),
    CONSTRAINT admin_audit_domain_check CHECK (domain IN (
        'content', 'telemetry', 'credits', 'rpc', 'registry',
        'explore', 'worlds', 'social', 'create', 'market', 'economy', 'ops'))
);

-- GET /admin/api/audit (newest first, paged) and its ?domain=/?action= filter.
CREATE INDEX admin_audit_at_idx ON ops.admin_audit (at DESC);
CREATE INDEX admin_audit_domain_action_idx ON ops.admin_audit (domain, action, at DESC);
-- The per-target history (an issue's audit drill-down, a wallet's operator history).
CREATE INDEX admin_audit_target_idx ON ops.admin_audit (target, at DESC) WHERE target IS NOT NULL;
-- ?actor= (what one operator did).
CREATE INDEX admin_audit_actor_idx ON ops.admin_audit (actor, at DESC);

-- ==== section: credits ====
-- The CreditUsed usage index (src/credits/usage.rs): the CreditsManager contract's
-- `CreditUsed(address indexed _sender, bytes32 indexed _creditId, (uint256
-- value, uint256 expiresAt, bytes32 salt) _credit, uint256 _value)` logs,
-- read with eth_getLogs over the collections chain (RPC_ENDPOINT_POLYGON)
-- up to its finalized block, one row per log. Chain facts, append-only:
-- a log is never updated, and a replay re-inserts the same key.
CREATE TABLE credits.usages (
    chain_id bigint NOT NULL,
    block_number bigint NOT NULL,
    log_index integer NOT NULL,
    contract text NOT NULL,
    tx_hash text NOT NULL,
    block_time timestamp without time zone,
    sender text NOT NULL,
    credit_id text NOT NULL,
    salt text NOT NULL,
    credit_value numeric(78,0) NOT NULL,
    credit_expires_at numeric(78,0) NOT NULL,
    value_wei numeric(78,0) NOT NULL,
    indexed_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT usages_pkey PRIMARY KEY (chain_id, block_number, log_index),
    CONSTRAINT usages_contract_check CHECK (contract ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT usages_sender_check CHECK (sender ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT usages_credit_id_check CHECK (credit_id ~ '^0x[0-9a-f]{64}$'),
    CONSTRAINT usages_salt_check CHECK (salt ~ '^0x[0-9a-f]{64}$'),
    CONSTRAINT usages_tx_hash_check CHECK (tx_hash ~ '^0x[0-9a-f]{64}$')
);

-- The PK serves the fold's incremental range (chain_id = $1 AND
-- block_number > $since AND block_number <= $through). This one serves the
-- other direction: the usages of one salt (a credit this node signed: the
-- reconciler's full pass and GET /credits/usage-index/status's lookups).
CREATE INDEX usages_salt_idx ON credits.usages (salt);

-- The indexer's durable progress, one row per chain: `last_block` is what
-- the log holds (advanced in the transaction that inserts a range's rows),
-- `folded_block` the block the last published fold covered, with
-- `folded_block_time` its chain time (the bound of the authorization expiry
-- sweep: a pledge is expired only once the index has read past its
-- expiry) and `folded_at` the wall clock of that publish (readers trust
-- the index while it is at most 10 minutes old and folded_block =
-- last_block). `contract` refuses a node reconfigured to another manager
-- without a replay.
CREATE TABLE credits.usage_index_cursor (
    chain_id bigint NOT NULL,
    contract text NOT NULL,
    last_block bigint NOT NULL,
    folded_block bigint,
    folded_block_time timestamp without time zone,
    folded_at timestamp without time zone,
    full_replay_started boolean NOT NULL DEFAULT false,
    updated_at timestamp without time zone NOT NULL DEFAULT timezone('UTC', now()),
    CONSTRAINT usage_index_cursor_pkey PRIMARY KEY (chain_id),
    CONSTRAINT usage_index_cursor_contract_check CHECK (contract ~ '^0x[0-9a-f]{40}$'),
    CONSTRAINT usage_index_cursor_folded_check CHECK (folded_block IS NULL OR folded_block <= last_block)
);
-- ==== end section: credits ====

-- ==== section: ops ====
-- (ops agent: a persisted job report or sync-control row if a restart must
-- keep them; both are process state today)
-- ==== end section: ops ====

-- Seasonal goal rewards are MANA wei, independent of the paid USD ledger.
CREATE TABLE credits.seasons (
    id integer PRIMARY KEY,
    name text NOT NULL,
    start_date timestamp without time zone NOT NULL,
    end_date timestamp without time zone NOT NULL,
    max_mana_wei numeric(78,0) NOT NULL CHECK (max_mana_wei >= 0),
    grace_days integer NOT NULL DEFAULT 14 CHECK (grace_days BETWEEN 0 AND 365),
    CHECK (end_date > start_date)
);
CREATE TABLE credits.season_goals (
    season_id integer NOT NULL REFERENCES credits.seasons(id),
    goal_id text NOT NULL CHECK (goal_id = 'take-photo'),
    title text NOT NULL,
    description text,
    thumbnail text NOT NULL,
    total_steps integer NOT NULL CHECK (total_steps > 0),
    reward_wei numeric(78,0) NOT NULL CHECK (reward_wei >= 0),
    start_week integer NOT NULL CHECK (start_week > 0),
    end_week integer NOT NULL CHECK (end_week >= start_week),
    position integer NOT NULL DEFAULT 0,
    PRIMARY KEY (season_id, goal_id)
);
CREATE TABLE credits.program_opt_outs (
    address text PRIMARY KEY REFERENCES credits.wallets(address),
    opted_out_at timestamp without time zone NOT NULL
);
CREATE TABLE credits.goal_progress (
    address text NOT NULL REFERENCES credits.wallets(address),
    season_id integer NOT NULL,
    week_id integer NOT NULL CHECK (week_id > 0),
    goal_id text NOT NULL,
    title text NOT NULL,
    description text,
    thumbnail text NOT NULL,
    total_steps integer NOT NULL CHECK (total_steps > 0),
    reward_wei numeric(78,0) NOT NULL CHECK (reward_wei >= 0),
    completed_steps integer NOT NULL DEFAULT 0 CHECK (completed_steps >= 0 AND completed_steps <= total_steps),
    completed_at timestamp without time zone,
    PRIMARY KEY (address, season_id, week_id, goal_id),
    FOREIGN KEY (season_id, goal_id) REFERENCES credits.season_goals(season_id, goal_id),
    CHECK ((completed_steps = total_steps) = (completed_at IS NOT NULL))
);
CREATE TABLE credits.photo_goal_receipts (
    photo_id uuid PRIMARY KEY,
    event_id bigint NOT NULL UNIQUE,
    address text NOT NULL,
    processed_at timestamp without time zone NOT NULL,
    outcome text NOT NULL,
    season_id integer,
    week_id integer,
    ordinal integer,
    CHECK ((outcome = 'counted') = (ordinal IS NOT NULL))
);
-- Wallet progress reads counted photo identities in completion order.
CREATE INDEX photo_goal_receipts_counted_wallet_idx
    ON credits.photo_goal_receipts (address, season_id, week_id, ordinal)
    WHERE outcome = 'counted';

CREATE TABLE credits.goal_grants (
    address text NOT NULL,
    season_id integer NOT NULL,
    week_id integer NOT NULL,
    goal_id text NOT NULL,
    salt text NOT NULL UNIQUE CHECK (salt ~ '^0x[0-9a-f]{64}$'),
    chain_id bigint NOT NULL CHECK (chain_id > 0),
    contract text NOT NULL CHECK (contract ~ '^0x[0-9a-f]{40}$'),
    credit_hash text NOT NULL CHECK (credit_hash ~ '^0x[0-9a-f]{64}$'),
    amount_wei numeric(78,0) NOT NULL CHECK (amount_wei >= 0),
    signature text NOT NULL,
    created_at timestamp without time zone NOT NULL,
    expires_at timestamp without time zone NOT NULL,
    claimed_at timestamp without time zone,
    PRIMARY KEY (address, season_id, week_id, goal_id),
    UNIQUE (chain_id, contract, credit_hash),
    FOREIGN KEY (address, season_id, week_id, goal_id)
        REFERENCES credits.goal_progress(address, season_id, week_id, goal_id),
    CHECK (expires_at > created_at)
);

-- A wallet's failed slider attempts, expiring 24 hours after the last failure.
-- Independent of multi-account flags: captcha failures block claims, not photos.
CREATE TABLE credits.captcha_attempts (
    address text PRIMARY KEY CHECK (address ~ '^0x[0-9a-f]{40}$'),
    failed_attempts smallint NOT NULL CHECK (failed_attempts BETWEEN 1 AND 3),
    last_failed_at timestamp without time zone NOT NULL
);

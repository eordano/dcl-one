-- shop.notification_cursor admits the producers that read the `sales` index
-- (shop.sale_event): `sales:mint:matic` (CollectionStore primary sales →
-- item_sold), `sales:bid_accepted:matic` (ERC721BidV2 acceptances →
-- bid_accepted and item_sold) and `royalties:bid_accepted:matic`
-- (royalties_earned). schemas/30_shop.sql declares them in the CHECK; a
-- database created before them re-declares the constraint here. Each row is
-- seeded at the index's published head on its first pass, so no past sale is
-- announced. A mint's price comes from the store's `Bought` log into the
-- existing nullable shop.sale_event.price: rows kept before it stay NULL
-- (no rewrite) and are never announced, being below every seed.
ALTER TABLE IF EXISTS shop.notification_cursor DROP CONSTRAINT IF EXISTS notification_cursor_producer_known;
ALTER TABLE IF EXISTS shop.notification_cursor ADD CONSTRAINT notification_cursor_producer_known CHECK (producer IN ('transfer_received', 'royalties_earned', 'rental_started', 'rental_ended', 'sales:traded:ethereum', 'sales:traded:matic', 'sales:order_successful:matic', 'sales:mint:matic', 'sales:bid_accepted:matic', 'royalties:bid_accepted:matic'));

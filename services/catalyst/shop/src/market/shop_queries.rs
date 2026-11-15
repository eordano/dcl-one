//! Shop queries implementation.

use super::*;

/// One source of listings shaped like the old `mv_trades` join: an open
/// trade whose sent asset resolves to a mirrored item, or a CollectionStore
/// mint. The columns are what every feed's filters and projection read.
fn branch_base(
    source: UnifiedSource,
    acquisition: UnifiedAcquisition,
    primary_only: bool,
) -> String {
    match acquisition {
        UnifiedAcquisition::Trade => {
            let asset_type = match source {
                UnifiedSource::Native => ASSET_TYPE_USD_PEGGED_MANA,
                UnifiedSource::Legacy => ASSET_TYPE_ERC20,
            };
            let primary = if primary_only {
                " AND u.type = 'public_item_order'"
            } else {
                ""
            };
            format!(
                "SELECT '{source}'::text AS source, 'trade'::text AS acquisition, u.id::text AS trade_id, u.type AS trade_type, u.signer, \
                        i.contract_address, i.item_id::text AS item_id, u.token_id::text AS token_id, i.name, i.image, i.rarity, i.item_type, \
                        i.category AS wearable_category, i.body_shapes, i.emote_loop, i.emote_outcome_type, i.emote_has_sound, i.emote_has_geometry, i.creator, \
                        CASE WHEN u.type = 'public_item_order' THEN i.available END AS available, \
                        u.amount_received::numeric AS amount, i.network, u.created_at, i.search_is_collection_approved AS approved, \
                        u.chain_id, lower(u.trade_contract) AS trade_contract, \
                        CASE WHEN u.type = 'public_nft_order' THEN u.signer END AS seller, \
                        CASE WHEN u.token_id IS NOT NULL THEN mod(u.token_id::numeric, {shift}::numeric)::text END AS issued_id \
                 FROM unified_trades u \
                 JOIN shop.items i ON i.contract_address = u.contract_address_sent AND i.item_id = u.item_id AND i.network = lower(u.network) \
                 WHERE u.received_asset_type = {asset_type} AND (u.type <> 'public_item_order' OR i.available > 0){primary}",
                source = source.as_str(),
                shift = ITEM_ID_SHIFT,
            )
        }
        UnifiedAcquisition::Store => format!(
            "SELECT 'legacy'::text AS source, 'store'::text AS acquisition, i.contract_address || '-' || i.item_id::text AS trade_id, 'public_item_order'::text AS trade_type, i.creator AS signer, \
                    i.contract_address, i.item_id::text AS item_id, NULL::text AS token_id, i.name, i.image, i.rarity, i.item_type, \
                    i.category AS wearable_category, i.body_shapes, i.emote_loop, i.emote_outcome_type, i.emote_has_sound, i.emote_has_geometry, i.creator, \
                    i.available, i.price::numeric AS amount, i.network, (to_timestamp(i.created_at) AT TIME ZONE 'UTC') AS created_at, i.search_is_collection_approved AS approved, \
                    NULL::integer AS chain_id, NULL::text AS trade_contract, \
                    NULL::text AS seller, NULL::text AS issued_id \
             FROM shop.items i \
             WHERE i.search_is_store_minter = true AND i.search_is_collection_approved = true AND i.available > 0 AND i.price > 0 \
               AND i.price <> {sentinel}::numeric AND i.emote_outcome_type IS NULL AND i.network <> 'ethereum'",
            sentinel = MAX_NUMERIC_NUMBER,
        ),
    }
}

const GENDER_EXPR: &str = "CASE WHEN mv.item_type = 'emote_v1' THEN NULL \
                                WHEN mv.body_shapes @> ARRAY['BaseMale','BaseFemale']::text[] THEN 'unisex' \
                                WHEN mv.body_shapes @> ARRAY['BaseMale']::text[] THEN 'male' \
                                WHEN mv.body_shapes @> ARRAY['BaseFemale']::text[] THEN 'female' \
                                ELSE NULL END";

/// The campaign selection: whole collections, individual items, or both
/// UNIONED. Fails closed (an empty page, never the unfiltered catalogue) only
/// when both named sets resolved empty. The item half leads with the
/// contracts so the index narrows before the composite matches exactly.
pub(super) fn unified_selection(
    b: &mut Builder,
    unified: &UnifiedCatalogFilters,
) -> Option<String> {
    if unified.contract_addresses.is_none() && unified.item_ids.is_none() {
        return None;
    }
    let collections = unified.contract_addresses.as_deref().unwrap_or_default();
    let items = unified.item_ids.as_deref().unwrap_or_default();
    if collections.is_empty() && items.is_empty() {
        return Some("FALSE".to_string());
    }
    let mut alternatives = Vec::new();
    if !collections.is_empty() {
        let p = b.bind_strings(
            &collections
                .iter()
                .map(|a| a.to_lowercase())
                .collect::<Vec<_>>(),
        );
        alternatives.push(format!("mv.contract_address = ANY({p})"));
    }
    if !items.is_empty() {
        let mut item_contracts: Vec<String> = items
            .iter()
            .filter_map(|id| id.rsplit_once('-').map(|(c, _)| c.to_lowercase()))
            .collect();
        item_contracts.sort();
        item_contracts.dedup();
        let contracts_p = b.bind_strings(&item_contracts);
        let items_p = b.bind_strings(&items.iter().map(|id| id.to_lowercase()).collect::<Vec<_>>());
        alternatives.push(format!("(mv.contract_address = ANY({contracts_p}) AND mv.contract_address || '-' || mv.item_id::text = ANY({items_p}))"));
    }
    Some(format!("({})", alternatives.join(" OR ")))
}

/// The filters every feed shares, over the `mv` alias of [`branch_base`].
fn append_unified_filters(
    b: &mut Builder,
    wheres: &mut Vec<String>,
    unified: &UnifiedCatalogFilters,
) {
    let f = &unified.base;
    wheres.push("mv.approved = true".to_string());
    if let Some(ca) = f.contract_address.as_deref().filter(|c| !c.is_empty()) {
        let p = b.bind_string(ca.to_lowercase());
        wheres.push(format!("mv.contract_address = {p}"));
    }
    if let Some(selection) = unified_selection(b, unified) {
        wheres.push(selection);
    }
    if let Some(iid) = &f.item_id {
        let p = b.bind_string(iid.clone());
        wheres.push(format!("mv.item_id = {p}"));
    }
    if !f.creator.is_empty() {
        let p = b.bind_strings(
            &f.creator
                .iter()
                .map(|c| c.to_lowercase())
                .collect::<Vec<_>>(),
        );
        wheres.push(format!("lower(mv.creator) = ANY({p})"));
    }
    match f.category.as_deref() {
        Some("emote") => wheres.push("mv.item_type ILIKE 'emote%'".to_string()),
        Some("wearable") => wheres.push("mv.item_type NOT ILIKE 'emote%'".to_string()),
        _ => {}
    }
    if !f.rarities.is_empty() {
        let p = b.bind_strings(
            &f.rarities
                .iter()
                .map(|r| r.to_lowercase())
                .collect::<Vec<_>>(),
        );
        wheres.push(format!("lower(mv.rarity) = ANY({p})"));
    }
    if !f.wearable_categories.is_empty() {
        let p = b.bind_strings(
            &f.wearable_categories
                .iter()
                .map(|c| c.to_lowercase())
                .collect::<Vec<_>>(),
        );
        wheres.push(format!("lower(mv.wearable_category) = ANY({p})"));
    }
    if f.is_smart {
        wheres.push("mv.item_type = 'smart_wearable_v1'".to_string());
    }
    // A wearable's emote columns are NULL, so these keep emotes only.
    if let Some(looped) = f.emote_loop {
        wheres.push(format!("mv.emote_loop = {looped}"));
    }
    if f.emote_has_sound {
        wheres.push("mv.emote_has_sound = true".to_string());
    }
    if f.emote_has_geometry {
        wheres.push("mv.emote_has_geometry = true".to_string());
    }
    if let Some(shapes) = body_shapes_for_genders(&f.wearable_genders) {
        let p = b.bind_strings(&shapes);
        wheres.push(format!(
            "(mv.item_type <> 'emote_v1' AND mv.body_shapes @> {p}::text[])"
        ));
    }
    if let Some(term) = f.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        for word in term.split_whitespace().take(8) {
            let p = b.bind_string(format!("%{}%", escape_like(word)));
            wheres.push(format!("mv.name ILIKE {p}"));
        }
    }
    if !f.include_social_emotes {
        wheres.push("mv.emote_outcome_type IS NULL".to_string());
    }
    match f.listing_type {
        Some(ShopListingType::Primary) => {
            wheres.push("mv.trade_type = 'public_item_order'".to_string())
        }
        Some(ShopListingType::Secondary) => {
            wheres.push("mv.trade_type <> 'public_item_order'".to_string())
        }
        None => {}
    }
}

/// The chains this deployment sells on, each (chain, marketplace, coupon
/// manager) pairing of them: a coupon is redeemable only by the marketplace
/// version its manager is wired into, on the chain it was signed for.
pub(crate) fn coupon_pairings() -> Vec<(i64, String, String)> {
    use super::super::listings::{offchain_marketplace_v2, offchain_marketplace_v3};
    use crate::coupons::contracts::{CouponMarketplace, coupon_contracts};
    let served = [
        super::super::wire::polygon_chain_id() as i64,
        super::super::wire::ethereum_chain_id() as i64,
    ];
    served
        .into_iter()
        .flat_map(|chain| {
            coupon_contracts(chain)
                .into_iter()
                .filter_map(move |contracts| {
                    let marketplace = match contracts.marketplace {
                        CouponMarketplace::OffChainMarketplaceV3 => offchain_marketplace_v3(chain),
                        CouponMarketplace::OffChainMarketplaceV2 => offchain_marketplace_v2(chain),
                    }?;
                    Some((
                        chain,
                        marketplace.address.to_lowercase(),
                        contracts.coupon_manager.address.to_lowercase(),
                    ))
                })
        })
        .collect()
}

/// The best live creator coupon for the listing `mv`, as `cp`: signed by the
/// listing's signer against the manager paired with the marketplace the
/// listing settles on, on the listing's own chain, covering its collection,
/// inside its window, neither cancelled nor revoked, with uses left as of the
/// last on-chain read. Biggest discount wins, then the one ending soonest.
/// Primaries only: the coupon contract mints collection items. The pairings
/// are the static registry, inlined.
fn coupon_join() -> String {
    let pairings: Vec<String> = coupon_pairings()
        .into_iter()
        .map(|(chain, marketplace, manager)| format!("({chain}, '{marketplace}', '{manager}')"))
        .collect();
    let paired = if pairings.is_empty() {
        "FALSE".to_string()
    } else {
        format!(
            "(c.chain_id, mv.trade_contract, lower(c.coupon_manager)) IN (VALUES {})",
            pairings.join(", ")
        )
    };
    format!(
        " LEFT JOIN LATERAL ( \
           SELECT c.id, c.signer, c.coupon_manager, c.coupon_address, c.checks, c.discount_type, c.discount_ppm, c.root, \
                  c.collections, c.signature, c.expires_at, COALESCE(cs.uses, 0) AS used \
           FROM shop.coupons c \
           LEFT JOIN shop.coupon_state cs ON cs.coupon_id = c.id \
           WHERE mv.trade_type = 'public_item_order' \
             AND c.signer = lower(mv.signer) \
             AND c.chain_id = mv.chain_id \
             AND {paired} \
             AND c.effective_since <= timezone('UTC', now()) AND c.expires_at > timezone('UTC', now()) \
             AND lower(mv.contract_address) = ANY(c.collections) \
             AND c.discount_type = 1 AND c.discount_ppm > 0 AND c.discount_ppm < 1000000 \
             AND NOT COALESCE(cs.cancelled, false) AND NOT COALESCE(cs.revoked, false) \
             AND COALESCE(cs.uses, 0) < (c.checks->>'uses')::numeric \
           ORDER BY c.discount_ppm DESC, c.expires_at ASC, c.id ASC \
           LIMIT 1 \
         ) cp ON true"
    )
}

/// The coupon columns of a branch that can carry one: what the coupon
/// contract leaves (`price - floor(price * ppm / 1e6)`), the list price as
/// the compare-at, the units left at the sale price, and the buy-side payload.
const COUPON_COLUMNS: &str = "cp.id::text AS coupon_id, cp.discount_ppm AS coupon_discount_ppm, \
     EXTRACT(EPOCH FROM cp.expires_at)::bigint AS sale_ends_at, \
     CASE WHEN cp.id IS NOT NULL THEN LEAST((cp.checks->>'uses')::numeric - cp.used, \
          COALESCE(mv.available::numeric, (cp.checks->>'uses')::numeric - cp.used))::bigint END AS sale_units_left, \
     CASE WHEN cp.id IS NOT NULL THEN jsonb_build_object( \
          'id', cp.id, 'signer', cp.signer, 'couponManager', cp.coupon_manager, 'couponAddress', cp.coupon_address, \
          'checks', cp.checks, 'discountType', cp.discount_type, 'discount', cp.discount_ppm, 'root', cp.root, \
          'collections', to_jsonb(cp.collections), 'signature', cp.signature, 'used', cp.used) END AS coupon";
const NULL_COUPON_COLUMNS: &str = "NULL::text AS coupon_id, NULL::integer AS coupon_discount_ppm, NULL::bigint AS sale_ends_at, NULL::bigint AS sale_units_left, NULL::jsonb AS coupon";

/// One UNION branch: the base filtered, projected into the feed columns.
/// `rate_p` converts a MANA amount to USD wei; the native branch is USD already.
pub(super) fn unified_branch(
    b: &mut Builder,
    source: UnifiedSource,
    acquisition: UnifiedAcquisition,
    rate_p: Option<&str>,
    filters: &UnifiedCatalogFilters,
) -> String {
    let primary_only = source == UnifiedSource::Legacy
        && acquisition == UnifiedAcquisition::Trade
        && !filters.include_legacy_secondary;
    let base = branch_base(source, acquisition, primary_only);
    // Creator coupons discount native primaries only: the coupon contract
    // mints collection items and settles in USD-pegged MANA.
    let with_coupons = source == UnifiedSource::Native && acquisition == UnifiedAcquisition::Trade;
    let usd_wei = match (rate_p, with_coupons) {
        (Some(rate_p), _) => format!("(mv.amount * {rate_p}::numeric)"),
        (None, true) => {
            "COALESCE(mv.amount - FLOOR(mv.amount * cp.discount_ppm / 1000000), mv.amount)"
                .to_string()
        }
        (None, false) => "mv.amount".to_string(),
    };
    let (compare_at, coupon_columns, join) = if with_coupons {
        (
            "CASE WHEN cp.id IS NOT NULL THEN mv.amount END",
            COUPON_COLUMNS,
            coupon_join(),
        )
    } else {
        ("NULL::numeric", NULL_COUPON_COLUMNS, String::new())
    };
    let mana_wei = match source {
        UnifiedSource::Native => "NULL::text",
        UnifiedSource::Legacy => "mv.amount::text",
    };
    let mut wheres = Vec::new();
    append_unified_filters(b, &mut wheres, filters);
    match (filters.base.discounted, with_coupons) {
        (Some(true), true) => wheres.push("cp.id IS NOT NULL".to_string()),
        (Some(true), false) => wheres.push("FALSE".to_string()),
        (Some(false), true) => wheres.push("cp.id IS NULL".to_string()),
        _ => {}
    }
    format!(
        "SELECT mv.source, mv.acquisition, mv.trade_id, mv.trade_type, mv.signer, mv.contract_address, mv.item_id, mv.token_id, mv.name, mv.image, \
                mv.rarity, mv.item_type, mv.wearable_category, mv.emote_loop, mv.creator, mv.seller, mv.issued_id, \
                {usd_wei} AS usd_wei, {compare_at} AS compare_at_usd_wei, mv.available::text AS available, upper(mv.network) AS network, \
                (EXTRACT(EPOCH FROM mv.created_at) * 1000)::bigint AS created_at, {mana_wei} AS mana_wei, \
                {coupon_columns}, \
                {GENDER_EXPR} AS gender \
         FROM ({base}) mv{join} {where_clause}",
        where_clause = where_from(&wheres),
    )
}

/// The branches the request's `source` keeps, joined by UNION ALL.
fn unified_inner(b: &mut Builder, filters: &UnifiedCatalogFilters, rate: ManaUsdRate) -> String {
    let rate_p = (filters.source != Some(UnifiedSource::Native))
        .then(|| b.bind_string(rate_to_numeric_string(rate)));
    let branches = [
        (UnifiedSource::Native, UnifiedAcquisition::Trade),
        (UnifiedSource::Legacy, UnifiedAcquisition::Trade),
        (UnifiedSource::Legacy, UnifiedAcquisition::Store),
    ];
    branches
        .into_iter()
        .filter(|(source, _)| filters.source.is_none_or(|s| s == *source))
        .map(|(source, acquisition)| {
            let rate = match source {
                UnifiedSource::Native => None,
                UnifiedSource::Legacy => rate_p.as_deref(),
            };
            unified_branch(b, source, acquisition, rate, filters)
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
}

/// The outer price bounds, sort and page over the subquery `alias`.
fn outer_page_clauses(
    b: &mut Builder,
    alias: &str,
    f: &ShopCatalogFilters,
    mut wheres: Vec<String>,
) -> (String, String, String, String) {
    if let Some(bound) = f.min_price_credits.and_then(unified_min_price_bound_wei) {
        let p = b.bind_string(bound.to_string());
        wheres.push(format!("{alias}.usd_wei > {p}::numeric"));
    }
    if let Some(max_wei) = f.max_price_credits.and_then(credits_to_wei) {
        let p = b.bind_string(max_wei.to_string());
        wheres.push(format!("{alias}.usd_wei <= {p}::numeric"));
    }
    let explicit = f.sort_by.map(|s| s == ShopSortBy::Relevance);
    let relevance = match f.search.as_deref() {
        Some(search) if super::super::items::ranks_by_relevance(explicit, Some(search)) => {
            super::super::items::search_relevance_rank(b, &format!("{alias}.name"), search)
        }
        _ => None,
    };
    let sort_key = match f.sort_by {
        _ if relevance.is_some() => {
            format!("{}, {alias}.created_at DESC", relevance.unwrap_or_default())
        }
        Some(ShopSortBy::Cheapest) => format!("{alias}.usd_wei ASC"),
        Some(ShopSortBy::MostExpensive) => format!("{alias}.usd_wei DESC"),
        Some(ShopSortBy::Name) => format!("{alias}.name ASC"),
        Some(ShopSortBy::Discount) => format!(
            "{alias}.coupon_discount_ppm DESC NULLS LAST, {alias}.sale_ends_at ASC NULLS LAST, {alias}.created_at DESC"
        ),
        Some(ShopSortBy::Newest | ShopSortBy::Relevance) | None => {
            format!("{alias}.created_at DESC")
        }
    };
    let limit_p = b.bind_i64(shop_clamp_first(f.first));
    let offset_p = b.bind_i64(shop_clamp_skip(f.skip));
    (
        where_from(&wheres),
        format!("ORDER BY {sort_key}, {alias}.trade_id"),
        limit_p,
        offset_p,
    )
}

/// The `unified_trades` CTE at now, counting mirrored trades from `since`.
fn with_trades(b: &mut Builder, since: Option<chrono::NaiveDateTime>) {
    let now_p = b.bind_time(Utc::now().naive_utc());
    let since_p = b.bind_opt_time(since);
    b.push("WITH ");
    b.push(&unified_trades_cte(&now_p, &since_p));
    b.push(" ");
}

/// The trades floor of a feed read now ([`super::super::trades::mirrored_since`]).
pub(super) fn since(state: &MarketplaceState) -> Option<chrono::NaiveDateTime> {
    super::super::trades::mirrored_since(&state.settings, Utc::now().naive_utc())
}

fn compare_at_credits_expr(alias: &str) -> String {
    format!(
        "CASE WHEN {alias}.compare_at_usd_wei IS NOT NULL THEN CEIL({alias}.compare_at_usd_wei / {USD_WEI_PER_CREDIT}::numeric)::bigint END"
    )
}

pub fn build_shop_listings_sql(
    f: &ShopCatalogFilters,
    since: Option<chrono::NaiveDateTime>,
) -> Builder {
    let mut b = Builder::new();
    with_trades(&mut b, since);
    let unified = UnifiedCatalogFilters {
        base: f.clone(),
        source: Some(UnifiedSource::Native),
        contract_addresses: None,
        item_ids: None,
        include_legacy_secondary: false,
    };
    let inner = unified_branch(
        &mut b,
        UnifiedSource::Native,
        UnifiedAcquisition::Trade,
        None,
        &unified,
    );
    let (where_clause, order, limit_p, offset_p) = outer_page_clauses(
        &mut b,
        "sub",
        f,
        vec![
            "sub.usd_wei > 0".to_string(),
            format!("sub.usd_wei <= {MAX_USD_WEI}::numeric"),
        ],
    );
    b.push(&format!(
        "SELECT sub.*, CEIL(sub.usd_wei / {USD_WEI_PER_CREDIT}::numeric)::bigint AS price_credits, {compare_at} AS compare_at_credits, COUNT(*) OVER() AS total \
         FROM ({inner}) sub {where_clause} {order} LIMIT {limit_p} OFFSET {offset_p}",
        compare_at = compare_at_credits_expr("sub"),
    ));
    b
}

pub fn build_legacy_listings_sql(
    f: &LegacyCatalogFilters,
    since: Option<chrono::NaiveDateTime>,
) -> Builder {
    let mut b = Builder::new();
    with_trades(&mut b, since);
    let unified = UnifiedCatalogFilters {
        base: ShopCatalogFilters {
            category: f.category.clone(),
            rarities: f.rarities.clone(),
            wearable_categories: f.wearable_categories.clone(),
            search: f.search.clone(),
            listing_type: Some(ShopListingType::Primary),
            ..Default::default()
        },
        source: Some(UnifiedSource::Legacy),
        contract_addresses: None,
        item_ids: None,
        include_legacy_secondary: false,
    };
    let inner = unified_branch(
        &mut b,
        UnifiedSource::Legacy,
        UnifiedAcquisition::Trade,
        None,
        &unified,
    );
    let order = match f.sort_by {
        Some(ShopSortBy::Cheapest) => "ORDER BY sub.usd_wei ASC",
        Some(ShopSortBy::MostExpensive) => "ORDER BY sub.usd_wei DESC",
        Some(ShopSortBy::Name) => "ORDER BY sub.name ASC",
        _ => "ORDER BY sub.created_at DESC",
    };
    let limit_p = b.bind_i64(shop_clamp_first(f.first));
    let offset_p = b.bind_i64(shop_clamp_skip(f.skip));
    b.push(&format!("SELECT sub.*, COUNT(*) OVER() AS total FROM ({inner}) sub {order}, sub.trade_id LIMIT {limit_p} OFFSET {offset_p}"));
    b
}

pub fn build_importable_listings_sql(
    seller: &str,
    since: Option<chrono::NaiveDateTime>,
) -> Builder {
    let mut b = Builder::new();
    with_trades(&mut b, since);
    let unified = UnifiedCatalogFilters {
        source: Some(UnifiedSource::Legacy),
        include_legacy_secondary: true,
        ..Default::default()
    };
    let inner = unified_branch(
        &mut b,
        UnifiedSource::Legacy,
        UnifiedAcquisition::Trade,
        None,
        &unified,
    );
    let seller_p = b.bind_string(seller.to_lowercase());
    let limit_p = b.bind_i64(SHOP_MAX_PAGE_SIZE);
    b.push(&format!("SELECT sub.* FROM ({inner}) sub WHERE lower(sub.signer) = {seller_p} ORDER BY sub.created_at DESC, sub.trade_id LIMIT {limit_p}"));
    b
}

pub fn build_unified_listings_sql(
    f: &UnifiedCatalogFilters,
    rate: ManaUsdRate,
    since: Option<chrono::NaiveDateTime>,
) -> Builder {
    let mut b = Builder::new();
    with_trades(&mut b, since);
    let inner = unified_inner(&mut b, f, rate);
    let (where_clause, order, limit_p, offset_p) = outer_page_clauses(
        &mut b,
        "sub",
        &f.base,
        vec![
            "sub.usd_wei > 0".to_string(),
            format!("sub.usd_wei <= {MAX_USD_WEI}::numeric"),
        ],
    );
    b.push(&format!(
        "SELECT sub.*, CEIL(sub.usd_wei / {USD_WEI_PER_CREDIT}::numeric)::bigint AS price_credits, {compare_at} AS compare_at_credits, COUNT(*) OVER() AS total \
         FROM ({inner}) sub {where_clause} {order} LIMIT {limit_p} OFFSET {offset_p}",
        compare_at = compare_at_credits_expr("sub"),
    ));
    b
}

/// The UNION collapsed to one representative row per item: primary before
/// secondary, native before legacy, cheapest, trade before store; with the
/// per-item `listing_count` window. Callers wrap it as `d`.
fn item_unified_core(b: &mut Builder, f: &UnifiedCatalogFilters, rate: ManaUsdRate) -> String {
    let inner = unified_inner(b, f, rate);
    format!(
        "SELECT DISTINCT ON (f.contract_address, f.item_id) f.*, CEIL(f.usd_wei / {USD_WEI_PER_CREDIT}::numeric)::bigint AS price_credits, {compare_at} AS compare_at_credits \
         FROM (SELECT u.*, COUNT(*) OVER (PARTITION BY u.contract_address, u.item_id) AS listing_count FROM ({inner}) u WHERE u.usd_wei > 0 AND u.usd_wei <= {MAX_USD_WEI}::numeric) f \
         ORDER BY f.contract_address, f.item_id, (CASE WHEN f.trade_type = 'public_item_order' THEN 0 ELSE 1 END), (CASE WHEN f.source = 'native' THEN 0 ELSE 1 END), \
                  f.usd_wei ASC, (CASE WHEN f.acquisition = 'trade' THEN 0 ELSE 1 END), f.trade_id",
        compare_at = compare_at_credits_expr("f"),
    )
}

pub fn build_unified_items_sql(
    f: &UnifiedCatalogFilters,
    rate: ManaUsdRate,
    since: Option<chrono::NaiveDateTime>,
) -> Builder {
    let mut b = Builder::new();
    with_trades(&mut b, since);
    let core = item_unified_core(&mut b, f, rate);
    let (where_clause, order, limit_p, offset_p) =
        outer_page_clauses(&mut b, "d", &f.base, vec!["d.usd_wei > 0".to_string()]);
    b.push(&format!("SELECT d.*, COUNT(*) OVER() AS total FROM ({core}) d {where_clause} {order} LIMIT {limit_p} OFFSET {offset_p}"));
    b
}

/// The item core restricted to `<contract>-<itemId>` ids; order is the caller's.
pub fn build_items_by_ids_sql(
    ids: &[String],
    f: &UnifiedCatalogFilters,
    rate: ManaUsdRate,
    since: Option<chrono::NaiveDateTime>,
) -> Builder {
    let mut b = Builder::new();
    with_trades(&mut b, since);
    let core = item_unified_core(&mut b, f, rate);
    let ids_p = b.bind_strings(ids);
    b.push(&format!("SELECT d.* FROM ({core}) d WHERE d.usd_wei > 0 AND (d.contract_address || '-' || d.item_id) = ANY({ids_p})"));
    b
}

const RARITY_ORDER: &[&str] = &[
    "unique",
    "mythic",
    "exotic",
    "legendary",
    "epic",
    "rare",
    "uncommon",
    "common",
];
const UNKNOWN_RARITY_DISTANCE: usize = RARITY_ORDER.len();

fn rarity_rank(rarity: &str) -> Option<usize> {
    let lower = rarity.to_ascii_lowercase();
    RARITY_ORDER.iter().position(|tier| *tier == lower)
}

/// A distance from the anchor's tier; `None` when the anchor's rarity is
/// unknown, so the caller omits the key (a bare integer in ORDER BY is a column position).
pub(super) fn related_rarity_sort_key(reference_rarity: Option<&str>) -> Option<String> {
    let anchor = reference_rarity.and_then(rarity_rank)?;
    let mut expr = String::from("CASE lower(d.rarity)");
    for (rank, tier) in RARITY_ORDER.iter().enumerate() {
        expr.push_str(&format!(" WHEN '{tier}' THEN {}", anchor.abs_diff(rank)));
    }
    expr.push_str(&format!(" ELSE {UNKNOWN_RARITY_DISTANCE} END"));
    Some(expr)
}

pub(super) struct ReferenceItem {
    pub(super) category: &'static str,
    pub(super) wearable_category: Option<String>,
    pub(super) rarity: Option<String>,
}

#[allow(
    clippy::too_many_arguments,
    reason = "one argument per related-items query parameter"
)]
pub fn build_related_items_sql(
    contract_address: &str,
    item_id: &str,
    reference_category: &str,
    reference_wearable_category: Option<&str>,
    reference_rarity: Option<&str>,
    first: Option<i64>,
    rail: &RelatedItemsFilters,
    rate: ManaUsdRate,
    since: Option<chrono::NaiveDateTime>,
) -> Builder {
    let mut b = Builder::new();
    with_trades(&mut b, since);
    let f = UnifiedCatalogFilters {
        base: ShopCatalogFilters {
            category: Some(reference_category.to_string()),
            wearable_categories: reference_wearable_category
                .map(|c| vec![c.to_string()])
                .unwrap_or_default(),
            listing_type: rail.listing_type,
            ..Default::default()
        },
        source: None,
        contract_addresses: None,
        item_ids: None,
        include_legacy_secondary: rail.include_legacy_secondary,
    };
    let core = item_unified_core(&mut b, &f, rate);
    let addr_p = b.bind_string(contract_address.to_lowercase());
    let item_p = b.bind_string(item_id.to_string());
    let limit_p = b.bind_i64(related_clamp_first(first));
    let order = match related_rarity_sort_key(reference_rarity) {
        Some(rarity_expr) => format!("ORDER BY {rarity_expr}, d.created_at DESC, d.trade_id"),
        None => "ORDER BY d.created_at DESC, d.trade_id".to_string(),
    };
    b.push(&format!("SELECT d.* FROM ({core}) d WHERE d.usd_wei > 0 AND (d.contract_address <> {addr_p} OR COALESCE(d.item_id, '') <> {item_p}) {order} LIMIT {limit_p}"));
    b
}

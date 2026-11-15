//! Shop projection implementation.

use super::*;

pub(super) fn top_level_category(item_type: Option<&str>) -> &'static str {
    if item_type
        .map(|t| t.to_lowercase().starts_with("emote"))
        .unwrap_or(false)
    {
        "emote"
    } else {
        "wearable"
    }
}

pub(super) fn listing_type(trade_type: &str) -> &'static str {
    if trade_type == "public_item_order" {
        "primary"
    } else {
        "secondary"
    }
}

pub(super) fn parse_available(available: Option<&str>) -> i64 {
    available.and_then(|s| s.parse::<i64>().ok()).unwrap_or(1)
}

pub(super) fn opt<T>(r: &PgRow, col: &str) -> Option<T>
where
    T: for<'a> sqlx::Decode<'a, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    r.try_get::<Option<T>, _>(col).ok().flatten()
}

/// The sale half of a row: set only when a coupon applies AND the list
/// price still beats the sale price once both round up to whole credits, so
/// a card never shows a discount the buyer cannot see. A coupon whose proof
/// for this collection cannot be built is dropped (a sale the checkout cannot
/// settle is worse than none).
fn sale_fields(r: &PgRow) -> ShopSaleFields {
    let Some(mut coupon) = opt::<Value>(r, "coupon") else {
        return ShopSaleFields::default();
    };
    let price_credits: i64 = r.try_get("price_credits").unwrap_or(0);
    let compare_at = opt::<i64>(r, "compare_at_credits");
    let Some(compare_at) = compare_at.filter(|c| *c > price_credits) else {
        return ShopSaleFields::default();
    };
    let contract = opt::<String>(r, "contract_address").unwrap_or_default();
    let collections: Vec<String> = coupon["collections"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    match crate::coupons::merkle::collection_proof_hex(&collections, &contract) {
        Ok(proof) => coupon["proof"] = serde_json::json!(proof),
        Err(e) => {
            tracing::warn!(coupon = %coupon["id"], contract = %contract, error = %e, "shop: dropping a coupon whose proof cannot be built");
            return ShopSaleFields::default();
        }
    }
    ShopSaleFields {
        compare_at_credits: Some(compare_at),
        sale_ends_at: opt(r, "sale_ends_at"),
        sale_units_left: opt(r, "sale_units_left"),
        coupon: Some(coupon),
    }
}

pub(super) fn map_unified_listing(r: &PgRow) -> UnifiedListing {
    let (network, chain_id) = network_and_chain(&opt::<String>(r, "network").unwrap_or_default());
    let acquisition: String = r.get("acquisition");
    let trade_type: String = r.get("trade_type");
    let listing_type = listing_type(&trade_type);
    let token_id: Option<String> = opt(r, "token_id");
    UnifiedListing {
        source: r.get("source"),
        trade_id: if acquisition == "store" {
            None
        } else {
            Some(r.get("trade_id"))
        },
        acquisition,
        listing_type: listing_type.to_string(),
        contract_address: opt::<String>(r, "contract_address").unwrap_or_default(),
        item_id: opt(r, "item_id"),
        token_id: token_id.clone(),
        name: opt::<String>(r, "name").unwrap_or_default(),
        thumbnail: opt::<String>(r, "image").unwrap_or_default(),
        rarity: opt::<String>(r, "rarity")
            .as_deref()
            .unwrap_or("common")
            .to_lowercase(),
        category: top_level_category(opt::<String>(r, "item_type").as_deref()).to_string(),
        wearable_category: opt(r, "wearable_category"),
        emote_loop: opt(r, "emote_loop"),
        gender: opt(r, "gender"),
        creator: opt::<String>(r, "creator").unwrap_or_default(),
        seller: opt(r, "seller"),
        issued_id: opt(r, "issued_id"),
        price_credits: crate::guards::catalog::credit_price(
            listing_type,
            token_id.as_deref(),
            r.get("price_credits"),
        ),
        sale: sale_fields(r),
        mana_wei: opt(r, "mana_wei"),
        available: parse_available(opt::<String>(r, "available").as_deref()),
        network,
        chain_id,
        created_at: r.get("created_at"),
    }
}

pub(super) fn map_unified_item(r: &PgRow) -> UnifiedItem {
    let l = map_unified_listing(r);
    UnifiedItem {
        source: l.source,
        acquisition: l.acquisition,
        trade_id: l.trade_id,
        listing_type: l.listing_type,
        contract_address: l.contract_address,
        item_id: l.item_id,
        token_id: l.token_id,
        name: l.name,
        thumbnail: l.thumbnail,
        rarity: l.rarity,
        category: l.category,
        wearable_category: l.wearable_category,
        emote_loop: l.emote_loop,
        gender: l.gender,
        creator: l.creator,
        seller: l.seller,
        issued_id: l.issued_id,
        price_credits: l.price_credits,
        sale: l.sale,
        mana_wei: l.mana_wei,
        listing_count: r.get("listing_count"),
        available: l.available,
        network: l.network,
        chain_id: l.chain_id,
        created_at: l.created_at,
    }
}

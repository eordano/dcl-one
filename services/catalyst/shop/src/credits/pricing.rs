//! How a marketplace item becomes a Credits price. The old crate asked the
//! market service (`/v1/items`, `/v1/orders/open-by-items`, `/v1/trades/{id}`)
//! and the price service (`/api/v3/simple/price`) over HTTP; here the same
//! questions are module calls: the items mirror (`marketplace::items`), the
//! open-orders readers (`marketplace::orders::open_orders_by_items`), the
//! trades mirror (`marketplace::trades`) and the MANA spot archive
//! (`price::State`, `MANA_PRICE_MAX_STALENESS_SECS` honoured). [`Market`] is
//! that seam, so the tests drive the pricing with a fake catalogue.
//!
//! The arithmetic stays in PostgreSQL NUMERIC (`ceil(wei / 1e18 * mana_usd
//! * (1 + markup) / CREDIT_USD)`), byte-identical between the single and the
//! batched form, and every guard that decides whether a charge covers a
//! payment works on the decimal text, never an `f64`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use sqlx::Row;

use super::errors::ApiError;
use crate::shared::cache::TtlMap;

const ALLOWED_CATEGORIES: [&str; 2] = ["wearable", "emote"];

/// The peg (1 Credit = 0.10 USD, the figure the packs are seeded at) as the
/// SQL pricing paths bind it; `UsdCents::CENTS_PER_CREDIT` is the same fact.
pub const CREDIT_USD: &str = crate::types::UsdCents::CREDIT_USD;

#[derive(Debug, Clone)]
pub struct ItemInfo {
    pub item_id: String,
    pub urn: String,
    pub category: String,
    pub price_wei: String,
    pub contract_address: String,
    pub store_mintable: bool,
    pub creator: String,
    pub chain_id: u64,
}

#[derive(Debug, Clone)]
pub struct OpenOrder {
    pub token_id: String,
    pub price_wei: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingVenue {
    V2 { token_id: String },
    Trade { trade_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenListing {
    pub venue: ListingVenue,
    pub price_wei: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BasisKind {
    Primary,
    Secondary { token_id: String },
    Trade { trade_id: String },
}

impl BasisKind {
    pub fn is_single_listing(&self) -> bool {
        !matches!(self, BasisKind::Primary)
    }
}

#[derive(Debug, Clone)]
pub struct ChargeBasis {
    pub info: ItemInfo,
    pub basis_wei: String,
    pub kind: BasisKind,
}

#[derive(Debug, Clone)]
pub struct PricedItem {
    pub basis: ChargeBasis,
    pub credit_price: String,
}

/// What the fulfilment worker learns about a pinned v3 trade before accepting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TradeLookup {
    /// Open, with the signed trade as the broker expects it (`id`, `signer`,
    /// `signature`, `checks`, `sent`, `received`).
    Open(serde_json::Value),
    NotFound,
    Closed(String),
}

pub const MARKETPLACE_V2_POLYGON: &str = "0x480a0f4e360e8964e68858dd231c2922f1df45ef";

pub const TRADE_CONTRACT_POLYGON: &str = "0x540fb08edb56aae562864b390542c97f562825ba";

pub const ORDER_SCAN_PAGE: usize = 100;

pub const ORDER_SCAN_MAX_PAGES: usize = 20;

pub const QUOTE_ORDER_SCAN_MAX_PAGES: usize = 5;

/// One oracle read serves every quote/cart/checkout/authorize/topup/outbox caller for this long.
pub const MANA_USD_MEMO_TTL: Duration = Duration::from_secs(20);

/// An open order as the listing selection sees it (the marketplace `Order`
/// fields it reads, or the old market JSON's).
#[derive(Debug, Clone)]
pub struct ListingRow {
    pub contract_address: String,
    pub marketplace_address: String,
    pub status: String,
    pub expires_at_ms: i64,
    pub token_id: Option<String>,
    pub trade_id: Option<String>,
    pub price_wei: String,
    /// The listing's owner, who is paid when it fills.
    pub seller: String,
    /// A v3 listing's beneficiary, paid instead of the seller when set.
    pub payee: Option<String>,
}

/// The catalogue, the order book, the trade book and the MANA spot, as the
/// pricing needs them. `LocalMarket` answers from the marketplace and price
/// modules; a test hands in a fake.
#[async_trait]
pub trait Market: Send + Sync {
    /// Element `i` answers `pairs[i]` (lowercased collection, item id): the
    /// not-found / ambiguous / category errors per item, a batch-level
    /// failure (the mirror not ready) as the outer error.
    async fn items(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<Result<ItemInfo, ApiError>>, ApiError>;

    /// The open orders of the pairs' collections, cheapest first, at most
    /// `per_contract` per collection.
    async fn open_orders(
        &self,
        pairs: &[(String, String)],
        per_contract: usize,
    ) -> Result<Vec<ListingRow>, ApiError>;

    async fn trade(&self, trade_id: &str) -> Result<TradeLookup, ApiError>;

    /// The creator's coupon a primary trade's purchase offer carries, chosen
    /// by the server's rule (`mana::facts::primary_trade_coupon`), never the
    /// client's. `None` for anything but an item order.
    async fn trade_coupon(
        &self,
        trade: &crate::types::SignedTrade,
        trade_type: &str,
    ) -> Result<Option<crate::types::TradeCoupon>, ApiError>;

    /// The MANA/USD spot as a decimal string, refused when older than the staleness budget.
    async fn mana_usd(&self) -> Result<String, ApiError>;
}

/// The in-process sources: the items mirror (gated on a complete pass like
/// `/v1/items`), the open-orders readers, the trades table and
/// `economy.price_snapshots` through the price module.
pub struct LocalMarket {
    pub pool: PgPool,
    pub market: Option<Arc<crate::market::MarketplaceState>>,
    pub price: Option<Arc<crate::price::State>>,
    pub max_staleness_secs: i64,
}

fn envelope_message(resp: axum::response::Response) -> String {
    let status = resp.status();
    format!("marketplace source unavailable ({status})")
}

#[async_trait]
impl Market for LocalMarket {
    async fn items(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<Result<ItemInfo, ApiError>>, ApiError> {
        if pairs.is_empty() {
            return Ok(vec![]);
        }
        let Some(market) = self.market.as_ref() else {
            return Err(ApiError::service_unavailable(
                "the item catalogue is not configured on this node (MARKETPLACE_MIRRORS_ENABLED)",
            ));
        };
        crate::market::mirrors::require_priced(market)
            .await
            .map_err(|r| ApiError::service_unavailable(envelope_message(r)))?;
        let mut ids: Vec<String> = Vec::new();
        for (collection, item_id) in pairs {
            let id = format!("{}-{}", collection.to_ascii_lowercase(), item_id);
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let filters = crate::market::items::ItemFilters {
            ids: ids.clone(),
            first: Some((ids.len() * 2).clamp(1, 100) as i64),
            ..Default::default()
        };
        let (items, _) = crate::market::items::list_items(market, &filters)
            .await
            .map_err(|e| ApiError::Internal(format!("items mirror read failed: {e}")))?;
        let mut by_key: HashMap<(String, String), Vec<ItemInfo>> = HashMap::new();
        for item in items {
            let key = (
                item.contract_address.to_ascii_lowercase(),
                item.item_id.clone(),
            );
            by_key.entry(key).or_default().push(ItemInfo {
                item_id: item.item_id.clone(),
                urn: item.urn.clone(),
                category: item.category.as_str().to_string(),
                price_wei: item.price.clone(),
                contract_address: item.contract_address.to_ascii_lowercase(),
                store_mintable: item.is_on_sale && item.trade_id.is_none(),
                creator: item.creator.to_ascii_lowercase(),
                chain_id: u64::from(item.chain_id),
            });
        }
        Ok(pairs
            .iter()
            .map(|(collection, item_id)| {
                let key = (collection.to_ascii_lowercase(), item_id.clone());
                match by_key.get(&key).map(|v| v.as_slice()) {
                    None | Some([]) => Err(ApiError::not_found("item not found in catalog")),
                    Some([item]) => check_category(item.clone()),
                    Some(v) => Err(ApiError::Internal(format!(
                        "catalog returned {} items for contractAddress={collection}&itemId={item_id} (ambiguous)",
                        v.len()
                    ))),
                }
            })
            .collect())
    }

    async fn open_orders(
        &self,
        pairs: &[(String, String)],
        per_contract: usize,
    ) -> Result<Vec<ListingRow>, ApiError> {
        let Some(market) = self.market.as_ref() else {
            return Err(ApiError::service_unavailable(
                "the order book is not configured on this node (collections subgraphs)",
            ));
        };
        let orders =
            crate::market::orders::open_orders_by_items(market, pairs, per_contract.max(1) as i64)
                .await
                .map_err(|r| ApiError::service_unavailable(envelope_message(r)))?;
        Ok(orders
            .into_iter()
            .map(|o| ListingRow {
                contract_address: o.contract_address,
                marketplace_address: o.marketplace_address,
                status: o.status,
                expires_at_ms: o.expires_at,
                token_id: o.token_id,
                trade_id: o.trade_id,
                price_wei: o.price,
                seller: o.owner,
                payee: o.payee,
            })
            .collect())
    }

    async fn trade(&self, trade_id: &str) -> Result<TradeLookup, ApiError> {
        if uuid::Uuid::parse_str(trade_id).is_err() {
            return Ok(TradeLookup::NotFound);
        }
        // A named trade resolves whatever its copy's age: the chain checks
        // the purchase against the signature, its expiry and cancellations.
        let Some(trade) = crate::market::trades::get_trade_by_id(
            &self.pool,
            trade_id,
            None,
            crate::market::trades::TradeScope::Served,
        )
        .await
        .map_err(|e| ApiError::Internal(format!("trades read failed: {e}")))?
        else {
            return Ok(TradeLookup::NotFound);
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let expiration = trade
            .checks
            .get("expiration")
            .and_then(json_as_i64)
            .unwrap_or(0);
        let effective = trade
            .checks
            .get("effective")
            .and_then(json_as_i64)
            .unwrap_or(0);
        if expiration != 0 && expiration <= now_ms {
            return Ok(TradeLookup::Closed("expired".to_string()));
        }
        if effective > now_ms {
            return Ok(TradeLookup::Closed("not yet effective".to_string()));
        }
        Ok(TradeLookup::Open(serde_json::to_value(&trade).map_err(
            |e| ApiError::Internal(format!("trade encode failed: {e}")),
        )?))
    }

    async fn trade_coupon(
        &self,
        trade: &crate::types::SignedTrade,
        trade_type: &str,
    ) -> Result<Option<crate::types::TradeCoupon>, ApiError> {
        crate::mana::facts::primary_trade_coupon(&self.pool, trade, trade_type)
            .await
            .map_err(|e| ApiError::Internal(format!("coupon read failed: {e}")))
    }

    async fn mana_usd(&self) -> Result<String, ApiError> {
        // The marketplace's feed where it is wired: the rate the contract converts
        // with. It sizes credit charges (authorize, cart, top-up, reprice), so its
        // round keeps the spot's bound, `MANA_PRICE_MAX_STALENESS_SECS`, on the
        // slot's monotonic clock (`mana::feed_rate::current_for_charge`); the
        // catalog may still show an older one. Why a read failed is logged by the
        // refresher; the client gets the generic reason only.
        let max_round_age = u64::try_from(self.max_staleness_secs).unwrap_or(0);
        if let Some(feed) = crate::mana::feed_rate::current_for_charge(max_round_age) {
            return feed
                .map(|shop| shop.usd_per_mana())
                .map_err(|unavailable| ApiError::service_unavailable(unavailable.to_string()));
        }
        let Some(price) = self.price.as_ref() else {
            return Err(ApiError::service_unavailable(
                "the MANA price is not configured on this node (PRICE_POLL_ENABLED)",
            ));
        };
        let snapshot = price
            .prices
            .latest()
            .await
            .map_err(|e| ApiError::Internal(format!("price snapshot read failed: {e}")))?
            .ok_or_else(|| {
                ApiError::service_unavailable(
                    "no MANA price snapshot on this node yet (mana-price-poll)",
                )
            })?;
        snapshot_usd(&snapshot, chrono::Utc::now(), self.max_staleness_secs)
    }
}

/// A price snapshot's MANA/USD as a decimal string, refused once it is
/// older than `max_staleness_secs`. Its age is the price module's
/// (`PriceSnapshot::age_at`): from the provider's quote, or from the poll
/// when that is earlier, as every other spot reader measures it.
fn snapshot_usd(
    snapshot: &crate::price::store::PriceSnapshot,
    now: chrono::DateTime<chrono::Utc>,
    max_staleness_secs: i64,
) -> Result<String, ApiError> {
    let usd = snapshot
        .mana_usd
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| ApiError::Internal("price snapshot missing numeric 'usd'".into()))?;
    let age = i64::try_from(snapshot.age_at(now).as_secs()).unwrap_or(i64::MAX);
    if age > max_staleness_secs {
        return Err(ApiError::service_unavailable(format!(
            "MANA/USD oracle is stale (age {age}s exceeds {max_staleness_secs}s)"
        )));
    }
    serde_json::Number::from_f64(usd)
        .map(|n| n.to_string())
        .ok_or_else(|| ApiError::Internal("price snapshot 'usd' is not a finite number".into()))
}

fn check_category(item: ItemInfo) -> Result<ItemInfo, ApiError> {
    if !ALLOWED_CATEGORIES.contains(&item.category.as_str()) {
        return Err(ApiError::bad_request(format!(
            "item category '{}' is not purchasable (wearable/emote only)",
            item.category
        )));
    }
    Ok(item)
}

/// The pricing over one [`Market`]: the memoised oracle read, the credit
/// conversion and the charge-basis resolution per fulfilment mode.
pub struct Pricing {
    market: Arc<dyn Market>,
    markup_bps: i64,
    mana_usd: TtlMap<(), String>,
}

impl Pricing {
    pub fn new(market: Arc<dyn Market>, markup_bps: i64) -> Self {
        Self {
            market,
            markup_bps,
            mana_usd: TtlMap::new(MANA_USD_MEMO_TTL),
        }
    }

    pub fn markup_bps(&self) -> i64 {
        self.markup_bps
    }

    pub async fn fetch_item(&self, collection: &str, item_id: &str) -> Result<ItemInfo, ApiError> {
        self.market
            .items(&[(collection.to_string(), item_id.to_string())])
            .await?
            .pop()
            .unwrap_or_else(|| Err(ApiError::Internal("empty catalog batch".into())))
    }

    pub async fn fetch_items_batch(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<Result<ItemInfo, ApiError>>, ApiError> {
        self.market.items(pairs).await
    }

    /// The rate every credit price here converts at. The spot is memoized
    /// [`MANA_USD_MEMO_TTL`]; the marketplace's feed is already one slot the
    /// refresher keeps current, and memoizing it again would serve a rate the
    /// other Shop paths have since dropped.
    pub async fn fetch_mana_usd(&self) -> Result<String, ApiError> {
        if crate::mana::feed_rate::installed() {
            return self.market.mana_usd().await;
        }
        self.mana_usd
            .get_or_fetch((), || self.market.mana_usd())
            .await
    }

    pub async fn fetch_trade_coupon(
        &self,
        trade: &crate::types::SignedTrade,
        trade_type: &str,
    ) -> Result<Option<crate::types::TradeCoupon>, ApiError> {
        self.market.trade_coupon(trade, trade_type).await
    }

    pub async fn fetch_trade(&self, trade_id: &str) -> Result<TradeLookup, ApiError> {
        let id = trade_id.trim();
        if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        {
            return Err(ApiError::Internal(format!(
                "invalid trade id {trade_id:?} pinned on the line"
            )));
        }
        self.market.trade(id).await
    }

    pub async fn compute_credit_price(
        &self,
        pool: &PgPool,
        mana_wei: &str,
        mana_usd: &str,
    ) -> Result<String, ApiError> {
        let row = sqlx::query(
            "SELECT ceil( \
                 ($1::numeric / 1e18) * $2::numeric \
                 * (1 + ($3::numeric / 10000)) \
                 / $4::numeric \
             )::text AS credit_price",
        )
        .bind(mana_wei)
        .bind(mana_usd)
        .bind(self.markup_bps)
        .bind(CREDIT_USD)
        .fetch_one(pool)
        .await?;
        Ok(row.get::<String, _>("credit_price"))
    }

    /// Batched form of [`Self::compute_credit_price`]: `WITH ORDINALITY` pins
    /// element `i` of the result to the reprice of `weis[i]`.
    pub async fn compute_credit_prices_batch(
        &self,
        pool: &PgPool,
        weis: &[String],
        mana_usd: &str,
    ) -> Result<Vec<String>, ApiError> {
        if weis.is_empty() {
            return Ok(vec![]);
        }
        let rows = sqlx::query(
            "SELECT ceil( \
                 (t.wei::numeric / 1e18) * $2::numeric \
                 * (1 + ($3::numeric / 10000)) \
                 / $4::numeric \
             )::text AS credit_price \
             FROM unnest($1::text[]) WITH ORDINALITY AS t(wei, ord) \
             ORDER BY t.ord",
        )
        .bind(weis)
        .bind(mana_usd)
        .bind(self.markup_bps)
        .bind(CREDIT_USD)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("credit_price"))
            .collect())
    }

    /// The charge bases of many pairs: one catalogue batch and one order-book
    /// read. Element `i` answers `pairs[i]`; a batch-level failure is the outer error.
    /// `buyer`'s own listings are never picked.
    pub async fn fetch_charge_bases_batch(
        &self,
        pairs: &[(String, String)],
        mode: &str,
        max_pages: usize,
        buyer: Option<&str>,
    ) -> Result<Vec<Result<ChargeBasis, ApiError>>, ApiError> {
        let infos = self.fetch_items_batch(pairs).await?;
        let wants_listing = mode == "secondary" || mode == "auto";
        let listing_pairs: Vec<(String, String)> = infos
            .iter()
            .zip(pairs)
            .filter_map(|(info, (_, item_id))| {
                info.as_ref()
                    .ok()
                    .map(|info| (info.contract_address.clone(), item_id.clone()))
            })
            .collect();
        let listings = if wants_listing && !listing_pairs.is_empty() {
            self.fetch_open_listings_batch(&listing_pairs, max_pages, true, buyer)
                .await?
        } else {
            HashMap::new()
        };
        Ok(infos
            .into_iter()
            .zip(pairs)
            .map(|(info, (_, item_id))| {
                let info = info?;
                let open_listing = if wants_listing {
                    let key = (info.contract_address.to_ascii_lowercase(), item_id.clone());
                    listings.get(&key).cloned().flatten()
                } else {
                    None
                };
                let resolved = resolve_basis(mode, &info, open_listing)?;
                Ok(ChargeBasis {
                    info,
                    basis_wei: resolved.basis_wei,
                    kind: resolved.kind,
                })
            })
            .collect())
    }

    /// [`Self::fetch_charge_bases_batch`] plus one (memoised) oracle read and
    /// one batched credit conversion.
    pub async fn price_items_for_mode(
        &self,
        pool: &PgPool,
        pairs: &[(String, String)],
        mode: &str,
        max_pages: usize,
        buyer: Option<&str>,
    ) -> Result<Vec<Result<PricedItem, ApiError>>, ApiError> {
        let bases = self
            .fetch_charge_bases_batch(pairs, mode, max_pages, buyer)
            .await?;
        let weis: Vec<String> = bases
            .iter()
            .filter_map(|b| b.as_ref().ok())
            .map(|b| b.basis_wei.clone())
            .collect();
        let mut prices = if weis.is_empty() {
            Vec::new()
        } else {
            let mana_usd = self.fetch_mana_usd().await?;
            self.compute_credit_prices_batch(pool, &weis, &mana_usd)
                .await?
        }
        .into_iter();
        Ok(bases
            .into_iter()
            .map(|basis| {
                let basis = basis?;
                let credit_price = prices.next().ok_or_else(|| {
                    ApiError::Internal("price batch shorter than its input".into())
                })?;
                ensure_charge_covers_payment(&basis.basis_wei, &credit_price)?;
                Ok(PricedItem {
                    basis,
                    credit_price,
                })
            })
            .collect())
    }

    /// The cheapest fillable MarketplaceV2 listing of an item (the worker's
    /// fresh-selection path for unpinned secondary rows).
    pub async fn fetch_open_order(
        &self,
        collection: &str,
        item_id: &str,
        buyer: &str,
    ) -> Result<Option<OpenOrder>, ApiError> {
        let pairs = [(collection.to_ascii_lowercase(), item_id.to_string())];
        let listings = self
            .fetch_open_listings_batch(&pairs, ORDER_SCAN_MAX_PAGES, false, Some(buyer))
            .await?;
        Ok(listings
            .get(&pairs[0])
            .cloned()
            .flatten()
            .and_then(|l| match l.venue {
                ListingVenue::V2 { token_id } => Some(OpenOrder {
                    token_id,
                    price_wei: l.price_wei,
                }),
                ListingVenue::Trade { .. } => None,
            }))
    }

    /// The cheapest open listing per `(collection, item_id)` pair out of one
    /// order-book read; pairs whose item id is not decimal never match a token
    /// and resolve to `None`. Every requested pair is present in the result.
    pub async fn fetch_open_listings_batch(
        &self,
        pairs: &[(String, String)],
        max_pages: usize,
        include_trades: bool,
        buyer: Option<&str>,
    ) -> Result<HashMap<(String, String), Option<OpenListing>>, ApiError> {
        let mut out: HashMap<(String, String), Option<OpenListing>> = HashMap::new();
        let mut asked: Vec<(String, String)> = Vec::new();
        for (collection, item_id) in pairs {
            let key = (collection.to_ascii_lowercase(), item_id.clone());
            if is_decimal_item_id(item_id) && !out.contains_key(&key) {
                asked.push(key.clone());
            }
            out.entry(key).or_insert(None);
        }
        if asked.is_empty() {
            return Ok(out);
        }
        let orders = self
            .market
            .open_orders(&asked, max_pages * ORDER_SCAN_PAGE)
            .await?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        for (key, slot) in out.iter_mut() {
            *slot = select_listing_for_pair(&orders, &key.0, &key.1, now_ms, include_trades, buyer);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBasis {
    pub basis_wei: String,
    pub kind: BasisKind,
}

pub fn resolve_basis(
    mode: &str,
    info: &ItemInfo,
    open_listing: Option<OpenListing>,
) -> Result<ResolvedBasis, ApiError> {
    let listing = |l: OpenListing| ResolvedBasis {
        basis_wei: l.price_wei,
        kind: match l.venue {
            ListingVenue::V2 { token_id } => BasisKind::Secondary { token_id },
            ListingVenue::Trade { trade_id } => BasisKind::Trade { trade_id },
        },
    };
    let mint = || ResolvedBasis {
        basis_wei: info.price_wei.clone(),
        kind: BasisKind::Primary,
    };
    let mintable_for_charge = info.store_mintable && payment_is_positive(&info.price_wei);
    match mode {
        "secondary" => match open_listing {
            Some(l) => Ok(listing(l)),
            None => Err(ApiError::conflict(
                "no open marketplace listing to fulfil this item from \u{2014} it may have just been bought",
            )),
        },
        "primary" => {
            if mintable_for_charge {
                Ok(mint())
            } else if info.store_mintable {
                Err(ApiError::conflict(
                    "this item mints for free \u{2014} free mints aren't sold through checkout",
                ))
            } else {
                Err(ApiError::conflict(
                    "this item's mint is not available from its collection store right now \u{2014} \
                     it may be off sale or sold out",
                ))
            }
        }
        "auto" => match (open_listing, mintable_for_charge) {
            (Some(l), true) => {
                if mint_undercuts_listing(&info.price_wei, &l.price_wei) {
                    Ok(mint())
                } else {
                    Ok(listing(l))
                }
            }
            (Some(l), false) => Ok(listing(l)),
            (None, true) => Ok(mint()),
            (None, false) => Err(ApiError::conflict(
                "this item has no open marketplace listing and isn't mintable from its \
                 collection store right now \u{2014} it may have just sold out",
            )),
        },
        other => Err(ApiError::Internal(format!(
            "unsupported fulfillment mode {other:?} (expected \"secondary\", \"primary\", or \"auto\")"
        ))),
    }
}

fn mint_undercuts_listing(mint_wei: &str, listing_wei: &str) -> bool {
    match (
        mint_wei.trim().parse::<u128>(),
        listing_wei.trim().parse::<u128>(),
    ) {
        (Ok(mint), Ok(listing)) => mint < listing,
        _ => false,
    }
}

/// Shared prefix of `charge_is_positive` and `parse_nonneg_decimal`: trim,
/// split on the first `.`, accept only `[digits][.[digits]]` with at least one
/// span non-empty. Deliberately narrower than `CreditAmount`: no sign, no
/// exponent, no magnitude bound.
pub(crate) fn split_validated_decimal(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((int_part, frac_part))
}

pub fn charge_is_positive(credit_price: &str) -> bool {
    let Some((int_part, frac_part)) = split_validated_decimal(credit_price) else {
        return false;
    };
    int_part.bytes().chain(frac_part.bytes()).any(|b| b != b'0')
}

pub fn payment_is_positive(basis_wei: &str) -> bool {
    let s = basis_wei.trim();
    s.is_empty() || !s.bytes().all(|b| b == b'0')
}

pub fn ensure_charge_covers_payment(basis_wei: &str, credit_price: &str) -> Result<(), ApiError> {
    if payment_is_positive(basis_wei) && !charge_is_positive(credit_price) {
        return Err(ApiError::conflict(
            "this item cannot be safely priced in Credits right now \u{2014} please try again later",
        ));
    }
    Ok(())
}

fn venue_rank(venue: &ListingVenue) -> u8 {
    match venue {
        ListingVenue::V2 { .. } => 0,
        ListingVenue::Trade { .. } => 1,
    }
}

pub fn select_cheapest_listing(
    orders: &[ListingRow],
    item_id: &str,
    now_ms: i64,
    include_trades: bool,
    buyer: Option<&str>,
) -> Option<OpenListing> {
    let mut best: Option<(u128, OpenListing)> = None;
    for o in orders {
        // Credits never pay a wallet for its own listing, as seller or payee.
        if buyer.is_some_and(|b| {
            o.seller.eq_ignore_ascii_case(b)
                || o.payee
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(b))
        }) {
            continue;
        }
        let is_v2 = o
            .marketplace_address
            .eq_ignore_ascii_case(MARKETPLACE_V2_POLYGON);
        let is_trade = include_trades
            && o.marketplace_address
                .eq_ignore_ascii_case(TRADE_CONTRACT_POLYGON);
        if !is_v2 && !is_trade {
            continue;
        }
        if o.status != "open" {
            continue;
        }
        if o.expires_at_ms != 0 && o.expires_at_ms <= now_ms {
            continue;
        }
        let token_id = match o.token_id.as_deref() {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => continue,
        };
        if !token_matches_item(&token_id, item_id) {
            continue;
        }
        let venue = if is_v2 {
            ListingVenue::V2 { token_id }
        } else {
            match o
                .trade_id
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                Some(t) => ListingVenue::Trade {
                    trade_id: t.to_string(),
                },
                None => continue,
            }
        };
        let price = match o.price_wei.parse::<u128>() {
            Ok(p) if p > 0 => p,
            _ => continue,
        };
        let better = match &best {
            None => true,
            Some((bp, bl)) => {
                price < *bp || (price == *bp && venue_rank(&venue) < venue_rank(&bl.venue))
            }
        };
        if better {
            best = Some((
                price,
                OpenListing {
                    venue,
                    price_wei: o.price_wei.clone(),
                },
            ));
        }
    }
    best.map(|(_, listing)| listing)
}

/// One pair's answer out of a mixed-collection order list.
fn select_listing_for_pair(
    orders: &[ListingRow],
    collection: &str,
    item_id: &str,
    now_ms: i64,
    include_trades: bool,
    buyer: Option<&str>,
) -> Option<OpenListing> {
    let mine: Vec<ListingRow> = orders
        .iter()
        .filter(|o| o.contract_address.eq_ignore_ascii_case(collection))
        .cloned()
        .collect();
    select_cheapest_listing(&mine, item_id, now_ms, include_trades, buyer)
}

fn is_decimal_item_id(item_id: &str) -> bool {
    crate::types::parse_decimal(item_id.trim()).is_ok()
}

fn token_matches_item(token_id: &str, item_id: &str) -> bool {
    use alloy_primitives::U256;
    const ISSUED_ID_BITS: usize = 216;
    let (token_id, item_id) = (token_id.trim(), item_id.trim());
    if token_id.is_empty() || item_id.is_empty() {
        return false;
    }
    let (Ok(tok), Ok(item)) = (
        U256::from_str_radix(token_id, 10),
        U256::from_str_radix(item_id, 10),
    ) else {
        return false;
    };
    (tok >> ISSUED_ID_BITS) == item
}

fn json_as_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

#[cfg(all(test, feature = "ts"))]
mod ts_peg_export {
    use super::CREDIT_USD;

    #[test]
    fn export_bindings_credit_peg() {
        let credit_usd: f64 = CREDIT_USD.parse().expect("CREDIT_USD must be a decimal");
        let credits_per_usd = (1.0 / credit_usd).round() as i64;
        assert!(
            (credits_per_usd as f64 * credit_usd - 1.0).abs() < 1e-9,
            "CREDIT_USD must divide 1 USD into a whole number of Credits"
        );
        let content = format!(
            "// This file was generated by `export_bindings_credit_peg` \
             (src/credits/pricing.rs). Do not edit this file manually.\n\
             // The hardcoded Credits peg: 1 Credit = {CREDIT_USD} USDC.\n\
             \n\
             export const CREDIT_USD = {credit_usd};\n\
             export const CREDIT_USD_DECIMAL = \"{CREDIT_USD}\";\n\
             export const CREDITS_PER_USD = {credits_per_usd};\n",
        );
        let dir = std::env::var("TS_RS_EXPORT_DIR").unwrap_or_else(|_| "./bindings".into());
        let dir = std::path::Path::new(&dir).join("credits");
        std::fs::create_dir_all(&dir).expect("create export dir");
        std::fs::write(dir.join("CreditPeg.ts"), content).expect("write CreditPeg.ts");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_ages_from_its_earlier_timestamp() {
        let now = chrono::Utc::now();
        let snapshot =
            |taken_ago: i64, quoted_ago: Option<i64>| crate::price::store::PriceSnapshot {
                mana_usd: Some(0.25),
                mana_eth: None,
                mana_btc: None,
                market_cap_usd: None,
                volume_24h_usd: None,
                price_change_24h_pct: None,
                source_updated_at: quoted_ago.map(|s| now - chrono::Duration::seconds(s)),
                taken_at: now - chrono::Duration::seconds(taken_ago),
            };
        assert_eq!(
            snapshot_usd(&snapshot(100, None), now, 900).unwrap(),
            "0.25"
        );
        assert_eq!(
            snapshot_usd(&snapshot(100, Some(800)), now, 900).unwrap(),
            "0.25"
        );
        assert!(snapshot_usd(&snapshot(100, Some(901)), now, 900).is_err());
        assert!(snapshot_usd(&snapshot(900, None), now, 900).is_ok());
        assert!(snapshot_usd(&snapshot(901, None), now, 900).is_err());
        // A provider quote time ahead of the poll does not make the rate
        // fresher than the poll that took it.
        let skewed = snapshot(1000, Some(0));
        assert_eq!(skewed.age_at(now).as_secs(), 1000);
        let err = snapshot_usd(&skewed, now, 900).unwrap_err();
        assert!(
            format!("{err:?}").contains("age 1000s exceeds 900s"),
            "{err:?}"
        );
    }

    fn info(price_wei: &str, store_mintable: bool) -> ItemInfo {
        ItemInfo {
            item_id: "4".into(),
            urn:
                "urn:decentraland:matic:collections-v2:0xeede64bfaf8055492aa500846ec7c6e6a9f533d5:4"
                    .into(),
            category: "wearable".into(),
            price_wei: price_wei.into(),
            contract_address: "0xeede64bfaf8055492aa500846ec7c6e6a9f533d5".into(),
            store_mintable,
            creator: "0x00000000000000000000000000000000000000c1".into(),
            chain_id: 137,
        }
    }

    fn v2(price_wei: &str) -> OpenListing {
        OpenListing {
            venue: ListingVenue::V2 {
                token_id: "1".into(),
            },
            price_wei: price_wei.into(),
        }
    }

    #[test]
    fn secondary_needs_a_listing_primary_needs_a_paid_mint() {
        let err = resolve_basis("secondary", &info("2500000000000000000", true), None).unwrap_err();
        assert!(matches!(err, ApiError::Conflict(_)));
        assert_eq!(
            resolve_basis("secondary", &info("1", true), Some(v2("5")))
                .unwrap()
                .basis_wei,
            "5"
        );
        assert_eq!(
            resolve_basis("primary", &info("7", true), None)
                .unwrap()
                .kind,
            BasisKind::Primary
        );
        assert!(
            resolve_basis("primary", &info("0", true), None)
                .unwrap_err()
                .to_string()
                .contains("free")
        );
        assert!(resolve_basis("primary", &info("7", false), None).is_err());
        assert!(resolve_basis("tertiary", &info("7", true), None).is_err());
    }

    #[test]
    fn auto_prefers_the_cheaper_side_and_falls_back() {
        assert_eq!(
            resolve_basis("auto", &info("10", true), Some(v2("5")))
                .unwrap()
                .basis_wei,
            "5"
        );
        assert_eq!(
            resolve_basis("auto", &info("3", true), Some(v2("5")))
                .unwrap()
                .kind,
            BasisKind::Primary
        );
        assert_eq!(
            resolve_basis("auto", &info("0", false), Some(v2("5")))
                .unwrap()
                .basis_wei,
            "5"
        );
        assert_eq!(
            resolve_basis("auto", &info("3", true), None).unwrap().kind,
            BasisKind::Primary
        );
        assert!(resolve_basis("auto", &info("3", false), None).is_err());
    }

    /// `ITEM_ID << 216`: the token id a collections-v2 NFT minted from item 4 carries.
    const ITEM_TOKEN_ID: &str =
        "421249166674228746791672110734681729275580381602196445017243910144";

    const SELLER: &str = "0x5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";

    fn row(contract: &str, mkt: &str, price: &str, trade: Option<&str>) -> ListingRow {
        ListingRow {
            contract_address: contract.into(),
            marketplace_address: mkt.into(),
            status: "open".into(),
            expires_at_ms: 0,
            token_id: Some(ITEM_TOKEN_ID.into()),
            trade_id: trade.map(str::to_string),
            price_wei: price.into(),
            seller: SELLER.into(),
            payee: None,
        }
    }

    #[test]
    fn selection_takes_the_items_own_collections_cheapest_listing_and_ranks_v2_first() {
        let c = "0xeede64bfaf8055492aa500846ec7c6e6a9f533d5";
        let rows = vec![
            row(
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                MARKETPLACE_V2_POLYGON,
                "1",
                None,
            ),
            row(c, MARKETPLACE_V2_POLYGON, "2000000000000000000", None),
            row(c, MARKETPLACE_V2_POLYGON, "1000000000000000000", None),
            row(
                c,
                TRADE_CONTRACT_POLYGON,
                "1000000000000000000",
                Some("t-1"),
            ),
        ];
        let picked = select_listing_for_pair(&rows, c, "4", 0, true, None).unwrap();
        assert_eq!(picked.price_wei, "1000000000000000000");
        assert_eq!(
            picked.venue,
            ListingVenue::V2 {
                token_id: ITEM_TOKEN_ID.into()
            }
        );
        let mut expired = rows.clone();
        expired[2].expires_at_ms = 1;
        let picked =
            select_listing_for_pair(&expired, c, "4", 1_700_000_000_000, false, None).unwrap();
        assert_eq!(
            picked.price_wei, "2000000000000000000",
            "an expired order and (without trades) the trade are skipped"
        );
        assert!(
            select_listing_for_pair(&rows, c, "5", 0, true, None).is_none(),
            "another item's token never matches"
        );
        let mut own = rows.clone();
        own[2].seller = "0xB0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0".into();
        let picked = select_listing_for_pair(
            &own,
            c,
            "4",
            0,
            false,
            Some("0xb0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0"),
        )
        .unwrap();
        assert_eq!(
            picked.price_wei, "2000000000000000000",
            "the buyer's own cheaper listing is skipped for the next one"
        );
        assert!(
            select_listing_for_pair(&own[2..3], c, "4", 0, false, Some(&own[2].seller)).is_none(),
            "only the buyer's own listing: none to fill from"
        );
    }

    #[test]
    fn positivity_guards_and_staleness() {
        assert!(
            charge_is_positive("1.5") && !charge_is_positive("0.00") && !charge_is_positive("1e18")
        );
        assert!(payment_is_positive("1") && !payment_is_positive("000") && payment_is_positive(""));
        assert!(ensure_charge_covers_payment("10", "0").is_err());
        assert!(ensure_charge_covers_payment("0", "0").is_ok());
        assert!(ensure_charge_covers_payment("10", "3").is_ok());
        assert!(is_decimal_item_id("12") && !is_decimal_item_id("0xab") && !is_decimal_item_id(""));
        let max = alloy_primitives::U256::MAX.to_string();
        assert_eq!(max.len(), 78);
        assert!(is_decimal_item_id(&max) && is_decimal_item_id(&format!(" {max} ")));
        assert!(!is_decimal_item_id(&format!("{max}0")) && !is_decimal_item_id("1_0"));
    }
}

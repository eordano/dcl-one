//! Shop recommendations implementation.

use super::*;

/// Windowed sale count and settled volume per `<contract>-<itemId>`.
pub fn aggregate_sales(sales: &[Value]) -> HashMap<String, SaleSignal> {
    let mut out: HashMap<String, SaleSignal> = HashMap::new();
    for sale in sales {
        let (Some(contract), Some(item)) = (
            readers::opt_str_of(sale, "searchContractAddress"),
            readers::opt_str_of(sale, "searchItemId"),
        ) else {
            continue;
        };
        let entry = out
            .entry(format!("{}-{item}", contract.to_lowercase()))
            .or_default();
        entry.sales += 1;
        entry.volume = entry
            .volume
            .saturating_add(readers::str_of(sale, "price").parse::<u128>().unwrap_or(0));
    }
    out
}

/// The old two-pass ranking over the hydrated rows: the first 60% of the
/// slots by sale count, the rest by volume among what that pass left.
pub fn rank_trending(
    mut items: Vec<UnifiedItem>,
    signals: &HashMap<String, SaleSignal>,
    first: i64,
) -> Vec<TrendingItem> {
    let key = |i: &UnifiedItem| {
        format!(
            "{}-{}",
            i.contract_address,
            i.item_id.clone().unwrap_or_default()
        )
    };
    let signal = |i: &UnifiedItem| signals.get(&key(i)).cloned().unwrap_or_default();
    let sales_slots = (first as f64 * TRENDING_SALES_CUT).ceil() as i64;
    let volume_slots = first - sales_slots;
    items.sort_by(|a, b| {
        let (sa, sb) = (signal(a), signal(b));
        sb.sales
            .cmp(&sa.sales)
            .then(sb.volume.cmp(&sa.volume))
            .then(a.contract_address.cmp(&b.contract_address))
            .then(a.item_id.cmp(&b.item_id))
    });
    let rest = items.split_off((sales_slots.max(0) as usize).min(items.len()));
    let mut by_sales = items;
    let mut by_volume = rest;
    by_volume.sort_by(|a, b| {
        let (sa, sb) = (signal(a), signal(b));
        sb.volume
            .cmp(&sa.volume)
            .then(sb.sales.cmp(&sa.sales))
            .then(a.contract_address.cmp(&b.contract_address))
            .then(a.item_id.cmp(&b.item_id))
    });
    by_volume.truncate(volume_slots.max(0) as usize);
    by_sales.extend(by_volume);
    by_sales.truncate(first.max(0) as usize);
    by_sales
        .into_iter()
        .map(|item| {
            let trending_sales = signal(&item).sales;
            TrendingItem {
                item,
                trending_sales,
            }
        })
        .collect()
}

/// Personal rows the rail needs before it calls itself personalised.
pub(super) const MIN_PERSONAL_ROWS: usize = 4;
const MAX_SEEDS: usize = 20;
/// Favourites have their own budget: a browsing session's seeds must not
/// push the deliberate signal out of the profile.
const MAX_FAVORITES: i64 = 20;
pub(super) const MAX_EQUIPPED: usize = 30;
const MAX_EXCLUDE: usize = 20;
const MAX_HELD_SEEDS: i64 = 200;

/// Where a profile item came from, which decides both its weight and the
/// reason a row it explains gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeedSource {
    Owned,
    Favorite,
    Equipped,
    Seed,
}

impl SeedSource {
    /// Relative to a holding: what the avatar wears and what the caller saved
    /// say what it likes now; a viewed or carted seed is weaker intent.
    fn weight(self) -> f64 {
        match self {
            SeedSource::Equipped => 1.5,
            SeedSource::Favorite => 1.2,
            SeedSource::Owned => 1.0,
            SeedSource::Seed => 0.8,
        }
    }

    /// A seed was looked at, never held, so it never reads "because you have".
    pub(super) fn reason(self) -> &'static str {
        match self {
            SeedSource::Owned => "co_owned",
            SeedSource::Favorite => "favorite_similar",
            SeedSource::Equipped => "equipped_similar",
            SeedSource::Seed => "seed_similar",
        }
    }
}

/// `<contract>-<itemId>` validated rather than trusted (it reaches SQL).
fn normalize_item_id(value: &str) -> Option<String> {
    let (contract, item) = value.trim().rsplit_once('-')?;
    (super::super::is_valid_eth_address(contract)
        && !item.is_empty()
        && item.bytes().all(|b| b.is_ascii_digit()))
    .then(|| format!("{}-{item}", contract.to_lowercase()))
}

/// `urn:decentraland:{matic,amoy}:collections-v2:<contract>:<itemId>` as the
/// `<contract>-<itemId>` id; base avatars and anything else are no taste signal.
pub(super) fn urn_to_item_id(urn: &str) -> Option<String> {
    let rest = urn.trim().strip_prefix("urn:decentraland:")?;
    let rest = rest
        .strip_prefix("matic:")
        .or_else(|| rest.strip_prefix("amoy:"))?;
    let (contract, item) = rest.strip_prefix("collections-v2:")?.split_once(':')?;
    normalize_item_id(&format!("{contract}-{item}"))
}

/// A bounded, de-duplicated id list, each value in either spelling: the
/// equipped list is where the client holds URNs, and a URN is twice an id.
pub(super) fn item_id_values(p: &Params, key: &str, cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in p
        .get_list(key, &[])
        .into_iter()
        .flat_map(|v| v.split(',').map(str::to_string).collect::<Vec<_>>())
    {
        let id = if value.contains(':') {
            urn_to_item_id(&value)
        } else {
            normalize_item_id(&value)
        };
        if let Some(id) = id.filter(|id| !out.contains(id)) {
            out.push(id);
        }
        if out.len() >= cap {
            break;
        }
    }
    out
}

/// One entry per item, keeping its strongest signal rather than the sum, so
/// an item owned, favourited and worn cannot crowd out everything else.
pub(super) fn build_profile(
    owned: &[String],
    favorites: &[String],
    equipped: &[String],
    seeds: &[String],
) -> Vec<(String, SeedSource)> {
    let mut best: Vec<(String, SeedSource)> = Vec::new();
    let sources = [
        (owned, SeedSource::Owned),
        (favorites, SeedSource::Favorite),
        (equipped, SeedSource::Equipped),
        (seeds, SeedSource::Seed),
    ];
    for (ids, source) in sources {
        for id in ids {
            match best.iter_mut().find(|(have, _)| have == id) {
                Some(entry) if source.weight() > entry.1.weight() => entry.1 = source,
                Some(_) => {}
                None => best.push((id.clone(), source)),
            }
        }
    }
    best
}

/// A neighbour hydrated for the re-rank.
#[derive(Debug, Clone)]
pub(super) struct RailCandidate {
    pub(super) id: String,
    pub(super) collection: String,
    pub(super) creator: String,
    pub(super) sub_category: String,
    pub(super) is_wearable: bool,
    pub(super) trigger: Option<String>,
    pub(super) score: f64,
}

/// Per collection, per creator: co-ownership is strongest inside a drop, so
/// score order alone fills the rail from one collection.
const MAX_PER_COLLECTION: usize = 2;
const MAX_PER_CREATOR: usize = 3;
/// Rows one profile item may explain: three "because you have X" read as one
/// recommendation repeated, whatever the items are.
const MAX_PER_TRIGGER: usize = 2;
/// The wearable share when the profile implies none.
const DEFAULT_WEARABLE_RATIO: f64 = 0.7;

/// Re-ranks the scored head into the rail: caps per collection, creator and
/// explaining item, a wearable/emote mix following the profile, and no two
/// rows of one sub-category in a row. Relaxed in tiers when supply is short
/// (first the run-breaking, then the caps, finally pure score order), since a
/// short rail is the worst of the three.
pub(super) fn rerank_for_diversity(
    candidates: Vec<RailCandidate>,
    limit: usize,
    wearable_ratio: f64,
) -> Vec<RailCandidate> {
    let mut ordered = candidates;
    ordered.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    let ratio = if wearable_ratio.is_finite() && wearable_ratio > 0.0 {
        wearable_ratio
    } else {
        DEFAULT_WEARABLE_RATIO
    };
    let wearable_target = (limit as f64 * ratio).round() as usize;
    let emote_target = limit.saturating_sub(wearable_target);
    let mut picked: Vec<RailCandidate> = Vec::new();
    let mut per_collection: HashMap<String, usize> = HashMap::new();
    let mut per_creator: HashMap<String, usize> = HashMap::new();
    let mut per_trigger: HashMap<String, usize> = HashMap::new();
    let (mut wearables, mut emotes) = (0usize, 0usize);
    let mut last_sub_category: Option<String> = None;
    for tier in 0..3 {
        for candidate in &ordered {
            if picked.len() >= limit {
                return picked;
            }
            if picked.iter().any(|p| p.id == candidate.id) {
                continue;
            }
            if tier < 2 {
                let count = |m: &HashMap<String, usize>, k: &str| m.get(k).copied().unwrap_or(0);
                let exceeds_mix = if candidate.is_wearable {
                    wearables >= wearable_target && emotes < emote_target
                } else {
                    emotes >= emote_target && wearables < wearable_target
                };
                if (!candidate.collection.is_empty()
                    && count(&per_collection, &candidate.collection) >= MAX_PER_COLLECTION)
                    || (!candidate.creator.is_empty()
                        && count(&per_creator, &candidate.creator) >= MAX_PER_CREATOR)
                    || candidate
                        .trigger
                        .as_deref()
                        .is_some_and(|t| count(&per_trigger, t) >= MAX_PER_TRIGGER)
                    || exceeds_mix
                    || (tier == 0
                        && !candidate.sub_category.is_empty()
                        && last_sub_category.as_deref() == Some(candidate.sub_category.as_str()))
                {
                    continue;
                }
            }
            *per_collection
                .entry(candidate.collection.clone())
                .or_default() += 1;
            *per_creator.entry(candidate.creator.clone()).or_default() += 1;
            if let Some(trigger) = &candidate.trigger {
                *per_trigger.entry(trigger.clone()).or_default() += 1;
            }
            if candidate.is_wearable {
                wearables += 1;
            } else {
                emotes += 1;
            }
            last_sub_category =
                (!candidate.sub_category.is_empty()).then(|| candidate.sub_category.clone());
            picked.push(candidate.clone());
        }
    }
    picked
}

/// `/v3/catalog/suggested`: the co-ownership neighbours (`neighbours`) of a
/// weighted profile (the items the wallet holds, the caller's favourites,
/// the `equipped` ids or URNs, the request's `seeds`), minus the profile and
/// `exclude`, hydrated through the same item core as the grid and re-ranked
/// for variety, with the reason naming the item that contributed most
/// (`co_owned`, `favorite_similar`, `equipped_similar`, `seed_similar`); the
/// trending floor fills the rest. Fewer than four personal rows, or no
/// neighbours build yet, answer the floor alone (`personalized: false`).
/// The signature is optional and unlocks exactly the favourites, read only
/// for the address it proves (and only when `address` is absent or that
/// same one; a signed caller naming nobody means themselves). The old rail's
/// content neighbours and taste blend are not carried.

pub(super) async fn personal_rows(
    state: &MarketplaceState,
    p: &Params<'_>,
    verified: Option<&str>,
    filters: &UnifiedCatalogFilters,
    rate: ManaUsdRate,
    limit: usize,
) -> Result<Vec<SuggestedItem>, ApiError> {
    let wallet = p
        .get_address("address", true, None)
        .map(|w| w.to_lowercase());
    let favorites_for = verified.filter(|v| wallet.as_deref().is_none_or(|w| w == *v));
    let profile_for = wallet.as_deref().or(verified);
    let requested: Vec<String> = id_list(p, "seeds", MAX_SEEDS)
        .iter()
        .filter_map(|id| normalize_item_id(id))
        .collect();
    let equipped = item_id_values(p, "equipped", MAX_EQUIPPED);
    let exclude = id_list(p, "exclude", MAX_EXCLUDE);
    let held = match profile_for {
        Some(w) => super::super::neighbours::held_items(&state.pool, w, MAX_HELD_SEEDS).await?,
        None => Vec::new(),
    };
    let held_ids: Vec<String> = held.iter().map(|(_, c, i)| format!("{c}-{i}")).collect();
    let favorites: Vec<String> = match favorites_for {
        Some(address) => match super::super::lists::get_picks_by_list_id(
            &state.pool,
            super::super::lists::DEFAULT_LIST_ID,
            Some(address),
            MAX_FAVORITES,
            0,
        )
        .await
        {
            Ok((picks, _)) => picks
                .iter()
                .filter_map(|pick| normalize_item_id(&pick.item_id))
                .collect(),
            Err(e) => {
                tracing::warn!(error = ?e, "suggestions: could not read favorites");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let profile = build_profile(&held_ids, &favorites, &equipped, &requested);
    if profile.is_empty() {
        return Ok(Vec::new());
    }
    let (contracts, items): (Vec<String>, Vec<String>) = profile
        .iter()
        .filter_map(|(id, _)| id.rsplit_once('-'))
        .map(|(c, i)| (c.to_string(), i.to_string()))
        .unzip();
    let rows = sqlx::query(
        "SELECT DISTINCT ON (i.contract_address, i.item_id) i.network, i.contract_address, i.item_id::text AS item_id, i.item_type \
         FROM UNNEST($1::text[], $2::text[]) AS s(contract_address, item_id) \
         JOIN shop.items i ON i.contract_address = s.contract_address AND i.item_id = s.item_id::numeric \
         ORDER BY i.contract_address, i.item_id, i.network DESC",
    )
    .bind(&contracts)
    .bind(&items)
    .fetch_all(&state.pool)
    .await?;
    let mut seeds: Vec<(String, String, String, f64)> = Vec::new();
    let (mut wearable_weight, mut total_weight) = (0.0f64, 0.0f64);
    for r in &rows {
        let (contract, item): (String, String) = (r.get("contract_address"), r.get("item_id"));
        let Some((_, source)) = profile
            .iter()
            .find(|(id, _)| *id == format!("{contract}-{item}"))
        else {
            continue;
        };
        let is_wearable = !opt::<String>(r, "item_type")
            .unwrap_or_default()
            .to_lowercase()
            .starts_with("emote");
        total_weight += source.weight();
        if is_wearable {
            wearable_weight += source.weight();
        }
        seeds.push((r.get("network"), contract, item, source.weight()));
    }
    let wearable_ratio = if total_weight > 0.0 {
        wearable_weight / total_weight
    } else {
        0.0
    };
    let mut skip: Vec<String> = exclude;
    skip.extend(profile.iter().map(|(id, _)| id.clone()));
    let scored = super::super::neighbours::weighted_neighbours_of(
        &state.pool,
        &seeds,
        &skip,
        (limit * 3) as i64,
    )
    .await?;
    if scored.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = scored.iter().map(|s| s.id.clone()).collect();
    let hydrated = items_by_ids(state, &ids, filters, rate).await?;
    let by_id: HashMap<String, &UnifiedItem> = hydrated
        .iter()
        .map(|i| {
            (
                format!(
                    "{}-{}",
                    i.contract_address,
                    i.item_id.clone().unwrap_or_default()
                ),
                i,
            )
        })
        .collect();
    let candidates: Vec<RailCandidate> = scored
        .iter()
        .filter_map(|s| {
            let item = by_id.get(&s.id)?;
            Some(RailCandidate {
                id: s.id.clone(),
                collection: item.contract_address.clone(),
                creator: item.creator.clone(),
                sub_category: format!(
                    "{}:{}",
                    item.category,
                    item.wearable_category.clone().unwrap_or_default()
                ),
                is_wearable: item.category != "emote",
                trigger: Some(s.seed.clone()),
                score: s.score,
            })
        })
        .collect();
    Ok(rerank_for_diversity(candidates, limit, wearable_ratio)
        .into_iter()
        .filter_map(|c| {
            let item = (*by_id.get(&c.id)?).clone();
            let source = profile
                .iter()
                .find(|(id, _)| Some(id) == c.trigger.as_ref())
                .map(|(_, s)| *s)
                .unwrap_or(SeedSource::Seed);
            Some(SuggestedItem {
                item,
                reason: SuggestionReason {
                    kind: source.reason(),
                    item_id: c.trigger,
                    creator: None,
                },
                score: c.score,
            })
        })
        .collect())
}

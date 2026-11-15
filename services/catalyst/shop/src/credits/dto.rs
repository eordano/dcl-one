//! The wire shapes of the user routes (the sites' generated `credits/*.ts` bindings were derived from these; the `ts-rs` export is not carried).

use serde::{Deserialize, Serialize};

fn serialize_number<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.fract() == 0.0 && *value >= i64::MIN as f64 && *value < -(i64::MIN as f64) {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct SeasonsData {
    #[serde(rename = "lastSeason")]
    pub last_season: Option<SeasonSummary>,
    #[serde(rename = "currentSeason")]
    pub current_season: Option<CurrentSeasonInfo>,
    #[serde(rename = "nextSeason")]
    pub next_season: Option<SeasonSummary>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct CurrentSeasonInfo {
    pub season: SeasonData,
    pub week: Week,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
#[serde(rename_all = "camelCase")]
pub struct SeasonSummary {
    pub id: i32,
    pub name: String,
    pub start_date: String,
    pub end_date: String,
    pub max_mana: String,
}

#[derive(Debug, Default, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct SeasonData {
    pub id: i32,
    pub name: String,
    #[serde(rename = "startDate")]
    pub start_date: String,
    #[serde(rename = "endDate")]
    pub end_date: String,
    #[serde(rename = "maxMana")]
    pub max_mana: String,
    #[serde(rename = "timeLeft")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub time_left: i64,
    #[serde(rename = "amountOfWeeks")]
    pub amount_of_weeks: i32,
    pub state: String,
}

#[derive(Debug, Default, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct Week {
    #[serde(rename = "weekNumber")]
    pub week_number: i32,
    #[serde(rename = "timeLeft")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub time_left: u64,
    #[serde(rename = "startDate")]
    pub start_date: String,
    #[serde(rename = "endDate")]
    pub end_date: String,
    #[serde(rename = "secondsRemaining")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub seconds_remaining: u64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct CreditsProgramProgressResponse {
    pub season: ProgressSeason,
    #[serde(rename = "currentWeek")]
    pub current_week: ProgressWeek,
    pub user: UserData,
    pub credits: CreditsData,
    pub goals: Vec<GoalData>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
#[serde(rename_all = "camelCase")]
pub struct ProgressSeason {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub time_left: i64,
    pub start_date: String,
    pub end_date: String,
    pub season_state: String,
}
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
#[serde(rename_all = "camelCase")]
pub struct ProgressWeek {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub time_left: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct UserData {
    #[serde(rename = "hasStartedProgram")]
    pub has_started_program: bool,
}

/// Seasonal MANA presentation; independent from the paid USD balance.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct CreditsData {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    #[serde(serialize_with = "serialize_number")]
    pub available: f64,
    #[serde(rename = "expiresIn")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub expires_in: u64,
    #[serde(rename = "isBlockedForClaiming")]
    pub is_blocked_for_claiming: bool,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct GoalData {
    #[serde(rename = "isCompleted")]
    pub is_completed: bool,
    #[serde(rename = "completedAt")]
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub completed_at: Option<i64>,
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub thumbnail: String,
    pub progress: GoalProgressData,
    #[serde(serialize_with = "serialize_number")]
    pub reward: f64,
    #[serde(rename = "isClaimed")]
    pub is_claimed: bool,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct GoalProgressData {
    pub metadata: GoalMetadata,
    #[serde(rename = "totalSteps")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total_steps: u64,
    #[serde(rename = "completedSteps")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub completed_steps: u64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct GoalMetadata {
    #[serde(rename = "photosTaken")]
    pub photos_taken: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClaimCreditsBody {
    pub x: f64,

    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct ClaimCreditsResponse {
    pub ok: bool,
    pub message: String,
    #[serde(serialize_with = "serialize_number")]
    pub credits_granted: f64,
    #[serde(rename = "isBlockedForClaiming")]
    pub is_blocked_for_claiming: bool,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct UserCreditsResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reason: Option<CreditFlagReason>,
    #[serde(
        rename = "isBlockedForClaiming",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub is_blocked_for_claiming: Option<bool>,
    pub credits: Vec<UserCreditItem>,
    #[serde(rename = "totalCredits")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    #[serde(serialize_with = "serialize_number")]
    pub total_credits: f64,
    pub totals: CreditsTotals,
    pub usd: UsdCredits,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
#[serde(rename_all = "camelCase")]
pub struct CreditFlagReason {
    pub message: String,
    pub ip: Option<String>,
    pub linked_addresses: Vec<String>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct UserCreditItem {
    pub id: String,
    #[serde(rename = "userAddress")]
    pub user_address: String,
    pub amount: String,
    #[serde(rename = "availableAmount")]
    pub available_amount: String,
    pub status: String,
    pub contract: String,
    pub timestamp: String,
    pub signature: String,
    #[serde(rename = "seasonId")]
    pub season_id: i32,
    #[serde(rename = "goalId")]
    pub goal_id: String,
    #[serde(rename = "weekId")]
    pub week_id: i32,
    #[serde(rename = "claimedAt")]
    pub claimed_at: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    #[serde(rename = "creditSource")]
    pub credit_source: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct CreditsTotals {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    #[serde(serialize_with = "serialize_number")]
    pub expiring: f64,
    #[serde(rename = "nonExpiring")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    #[serde(serialize_with = "serialize_number")]
    pub non_expiring: f64,
}

/// The USD block (upstream `get-user-credits.ts`): `balanceCents` is the
/// SPENDABLE balance (the live pledges already subtracted), `credits` its
/// floor, `held` the pledges themselves when there are any (what the Shop's
/// credits page shows as "on hold").
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "credits/"))]
pub struct UsdCredits {
    #[serde(rename = "balanceCents")]
    pub balance_cents: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub credits: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub held: Option<super::history::HeldOut>,
}

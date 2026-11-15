use super::*;
use crate::credits::dto::*;
use chrono::SecondsFormat;
fn iso(t: NaiveDateTime) -> String {
    t.and_utc().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub async fn enroll(state: &SharedState, address: &str) -> Result<(), ApiError> {
    let mut tx = state.credits.pool.begin().await?;
    wallet_lock(&mut tx, address).await?;
    let email: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.subscriptions WHERE address=$1 AND email IS NOT NULL AND email <> '')")
        .bind(address).fetch_one(&mut *tx).await?;
    if !email {
        return Err(ApiError::bad_request(
            "User must be subscribed to notifications with a valid email",
        ));
    }
    sqlx::query("INSERT INTO credits.wallets(address,has_started_program) VALUES($1,true) ON CONFLICT(address) DO UPDATE SET has_started_program=true")
        .bind(address).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM credits.program_opt_outs WHERE address=$1")
        .bind(address)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn status(state: &SharedState, address: &str) -> Result<Value, ApiError> {
    let row = sqlx::query("SELECT COALESCE(w.has_started_program,false) AS started,o.opted_out_at FROM (VALUES($1::text)) q(address) LEFT JOIN credits.wallets w USING(address) LEFT JOIN credits.program_opt_outs o USING(address)")
        .bind(address).fetch_one(&state.credits.pool).await?;
    let opted: Option<NaiveDateTime> = row.get("opted_out_at");
    Ok(
        json!({"ok":true,"data":{"status":if opted.is_some(){"opted_out"}else if row.get::<bool,_>("started"){"enrolled"}else{"not_registered"},"optedOutAt":opted.map(iso)}}),
    )
}
pub async fn opt_out(state: &SharedState, address: &str) -> Result<(), ApiError> {
    let now = super::super::store::now();
    let mut tx = state.credits.pool.begin().await?;
    wallet_lock(&mut tx, address).await?;
    let started: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT has_started_program FROM credits.wallets WHERE address=$1),false)",
    )
    .bind(address)
    .fetch_one(&mut *tx)
    .await?;
    if !started {
        return Err(ApiError::not_found("User not found"));
    }
    let opted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM credits.program_opt_outs WHERE address=$1)",
    )
    .bind(address)
    .fetch_one(&mut *tx)
    .await?;
    if opted {
        return Err(ApiError::bad_request("User already unregistered"));
    }
    if let Some(season) = current(&mut tx, now).await? {
        let claimed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credits.goal_grants WHERE address=$1 AND claimed_at >= $2)")
            .bind(address).bind(season.week(now).1).fetch_one(&mut *tx).await?;
        if claimed {
            return Err(ApiError::bad_request(
                "Cannot unregister with credits claimed this week",
            ));
        }
    }
    sqlx::query("INSERT INTO credits.program_opt_outs(address,opted_out_at) VALUES($1,$2)")
        .bind(address)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Check source preconditions before consuming a captcha; claim repeats them
/// under its monetary locks after the external captcha provider has answered.
pub async fn claim_preflight(state: &SharedState, address: &str) -> Result<(), ApiError> {
    let now = super::super::store::now();
    let mut conn = state.credits.pool.acquire().await?;
    let season = current(&mut conn, now)
        .await?
        .ok_or_else(|| ApiError::bad_request("No active season found"))?;
    let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credits.goal_grants WHERE address=$1 AND season_id=$2 AND week_id=$3 AND claimed_at IS NULL AND expires_at>$4)").bind(address).bind(season.id).bind(season.week(now).0).bind(now).fetch_one(&mut *conn).await?;
    if !exists {
        return Err(ApiError::bad_request("No unclaimed credits found"));
    }
    let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credits.wallet_flags WHERE address=$1) OR COALESCE((SELECT is_blocked_for_claiming FROM credits.wallets WHERE address=$1),false)").bind(address).fetch_one(&mut *conn).await?;
    if blocked {
        return Err(blocked_error(&mut conn, address).await?);
    }
    Ok(())
}

pub async fn claim(state: &SharedState, address: &str) -> Result<f64, ApiError> {
    let now = super::super::store::now();
    let mut tx = state.credits.pool.begin().await?;
    // All claims lock the season before their wallet. Photo processing never locks a season.
    let season = current(&mut tx, now)
        .await?
        .ok_or_else(|| ApiError::bad_request("No active season found"))?;
    sqlx::query("SELECT id FROM credits.seasons WHERE id=$1 FOR UPDATE")
        .bind(season.id)
        .execute(&mut *tx)
        .await?;
    wallet_lock(&mut tx, address).await?;
    let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credits.wallet_flags WHERE address=$1) OR COALESCE((SELECT is_blocked_for_claiming FROM credits.wallets WHERE address=$1),false)")
        .bind(address).fetch_one(&mut *tx).await?;
    if blocked {
        return Err(blocked_error(&mut tx, address).await?);
    }
    let week = season.week(now).0;
    let amount: String = sqlx::query_scalar("SELECT COALESCE(sum(amount_wei),0)::text FROM credits.goal_grants WHERE address=$1 AND season_id=$2 AND week_id=$3 AND claimed_at IS NULL AND expires_at>$4")
        .bind(address).bind(season.id).bind(week).bind(now).fetch_one(&mut *tx).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM credits.goal_grants WHERE address=$1 AND season_id=$2 AND week_id=$3 AND claimed_at IS NULL AND expires_at>$4")
        .bind(address).bind(season.id).bind(week).bind(now).fetch_one(&mut *tx).await?;
    if count == 0 {
        return Err(ApiError::bad_request("No unclaimed credits found"));
    }
    let cap = sqlx::query("SELECT COALESCE(sum(amount_wei) FILTER(WHERE week_id=$2),0)+$3::numeric > $4::numeric AS weekly, COALESCE(sum(amount_wei),0)+$3::numeric > (SELECT max_mana_wei FROM credits.seasons WHERE id=$1) AS season FROM credits.goal_grants WHERE season_id=$1 AND claimed_at IS NOT NULL")
        .bind(season.id).bind(week).bind(&amount).bind(WEEKLY_CAP).fetch_one(&mut *tx).await?;
    if cap.get::<bool, _>("weekly") {
        return Err(ApiError::ClaimCap { weekly: true });
    }
    if cap.get::<bool, _>("season") {
        return Err(ApiError::ClaimCap { weekly: false });
    }
    sqlx::query("UPDATE credits.goal_grants SET claimed_at=$4 WHERE address=$1 AND season_id=$2 AND week_id=$3 AND claimed_at IS NULL AND expires_at>$4")
        .bind(address).bind(season.id).bind(week).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    mana_number(&amount)
}

pub async fn user_credits(
    state: &SharedState,
    address: &str,
    status: Option<&str>,
) -> Result<(Vec<UserCreditItem>, f64, u64), ApiError> {
    read_credits(state, address, status, true).await
}
async fn read_credits(
    state: &SharedState,
    address: &str,
    status: Option<&str>,
    hide_flagged: bool,
) -> Result<(Vec<UserCreditItem>, f64, u64), ApiError> {
    let now = super::super::store::now();
    let blocked: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM credits.wallet_flags WHERE address=$1)")
            .bind(address)
            .fetch_one(&state.credits.pool)
            .await?;
    if hide_flagged && blocked {
        return Ok((Vec::new(), 0.0, 0));
    }
    let rows = sqlx::query("SELECT g.*,g.amount_wei::text AS amount, GREATEST(g.amount_wei-COALESCE(u.used,0),0)::text AS available, CASE WHEN u.used IS NULL THEN 'AVAILABLE' WHEN u.used>=g.amount_wei THEN 'FULLY_USED' ELSE 'PARTIALLY_USED' END AS status FROM credits.goal_grants g LEFT JOIN LATERAL(SELECT sum(value_wei) AS used FROM credits.usages u WHERE u.salt=g.salt AND u.chain_id=g.chain_id AND u.contract=g.contract AND u.credit_id=g.credit_hash AND u.sender=g.address) u ON true WHERE g.address=$1 AND g.claimed_at IS NOT NULL AND g.expires_at>$2 ORDER BY g.created_at DESC")
        .bind(address).bind(now).fetch_all(&state.credits.pool).await?;
    let mut items = Vec::new();
    let mut total = U256::ZERO;
    let mut expiry = None;
    for row in rows {
        let available: String = row.get("available");
        let value = uint(&available)?;
        total = total
            .checked_add(value)
            .ok_or_else(|| ApiError::Internal("goal balance overflow".into()))?;
        let expires: NaiveDateTime = row.get("expires_at");
        expiry = Some(expiry.map_or(expires, |old: NaiveDateTime| old.max(expires)));
        let row_status: String = row.get("status");
        if status.map_or(row_status == "FULLY_USED", |filter| row_status != filter) {
            continue;
        }
        items.push(UserCreditItem {
            id: row.get("salt"),
            user_address: address.into(),
            amount: row.get("amount"),
            available_amount: available,
            status: row_status,
            contract: row.get("contract"),
            timestamp: row
                .get::<NaiveDateTime, _>("created_at")
                .and_utc()
                .timestamp_millis()
                .to_string(),
            signature: row.get("signature"),
            season_id: row.get("season_id"),
            goal_id: row.get("goal_id"),
            week_id: row.get("week_id"),
            claimed_at: row
                .get::<NaiveDateTime, _>("claimed_at")
                .and_utc()
                .timestamp_millis()
                .to_string(),
            expires_at: expires.and_utc().timestamp().to_string(),
            credit_source: "goal".into(),
        });
    }
    Ok((
        items,
        wire_number(&total.to_string())?,
        expiry
            .map(|t| (t - now).num_seconds().max(0) as u64)
            .unwrap_or(0),
    ))
}

async fn season_state(
    conn: &mut PgConnection,
    season: &Season,
    week: i32,
) -> Result<String, ApiError> {
    let row = sqlx::query("SELECT COALESCE(sum(amount_wei),0) >= $2::numeric AS season, COALESCE(sum(amount_wei) FILTER(WHERE week_id=$3),0) >= $4::numeric AS week FROM credits.goal_grants WHERE season_id=$1 AND claimed_at IS NOT NULL")
        .bind(season.id).bind(&season.max_mana_wei).bind(week).bind(WEEKLY_CAP).fetch_one(conn).await?;
    Ok(if row.get::<bool, _>("season") {
        "ERR_SEASON_RUN_OUT_OF_FUNDS"
    } else if row.get::<bool, _>("week") {
        "ERR_WEEK_RUN_OUT_OF_FUNDS"
    } else {
        "RUNNING"
    }
    .into())
}

pub async fn progress(
    state: &SharedState,
    address: &str,
) -> Result<CreditsProgramProgressResponse, ApiError> {
    let now = super::super::store::now();
    let is_paused = paused(state).await;
    let (has_started, wallet) = state.credits.user_progress(address).await?;
    let opted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM credits.program_opt_outs WHERE address=$1)",
    )
    .bind(address)
    .fetch_one(&state.credits.pool)
    .await?;
    let (_, total, expires) = read_credits(state, address, None, false).await?;
    let mut tx = state.credits.pool.begin().await?;
    let season = current(&mut tx, now).await?;
    let mut goals = Vec::new();
    let (season, current_week) = if let Some(s) = season {
        let (week, _, end) = s.week(now);
        // Reads initialize only registered identities; unregistered users get the same definition as a default.
        if crate::shared::writes::ops_refusal(state.settings.writes_enabled).is_none() {
            initialize(&mut tx, address, s.id, week).await?;
        }
        let rows = sqlx::query("SELECT d.goal_id,COALESCE(p.title,d.title) AS title,CASE WHEN p.address IS NULL THEN d.description ELSE p.description END AS description,COALESCE(p.thumbnail,d.thumbnail) AS thumbnail,COALESCE(p.total_steps,d.total_steps) AS total_steps,COALESCE(p.reward_wei,d.reward_wei)::text AS reward,COALESCE(p.completed_steps,0) AS completed_steps,p.completed_at,g.claimed_at FROM credits.season_goals d LEFT JOIN credits.goal_progress p ON p.address=$1 AND p.season_id=d.season_id AND p.week_id=$3 AND p.goal_id=d.goal_id LEFT JOIN credits.goal_grants g ON g.address=p.address AND g.season_id=p.season_id AND g.week_id=p.week_id AND g.goal_id=p.goal_id WHERE d.season_id=$2 AND (p.address IS NOT NULL OR $3 BETWEEN d.start_week AND d.end_week) ORDER BY d.position")
            .bind(address).bind(s.id).bind(week).fetch_all(&mut *tx).await?;
        let photos: Vec<Uuid> = sqlx::query_scalar("SELECT photo_id FROM credits.photo_goal_receipts WHERE address=$1 AND season_id=$2 AND week_id=$3 AND outcome='counted' ORDER BY ordinal")
            .bind(address).bind(s.id).bind(week).fetch_all(&mut *tx).await?;
        for row in rows {
            let completed: Option<NaiveDateTime> = row.get("completed_at");
            goals.push(GoalData {
                is_completed: completed.is_some(),
                completed_at: completed.map(|t| t.and_utc().timestamp_millis()),
                id: row.get("goal_id"),
                title: row.get("title"),
                description: row.get("description"),
                thumbnail: row.get("thumbnail"),
                reward: mana_number(&row.get::<String, _>("reward"))?,
                is_claimed: row.get::<Option<NaiveDateTime>, _>("claimed_at").is_some(),
                progress: GoalProgressData {
                    total_steps: row.get::<i32, _>("total_steps") as u64,
                    completed_steps: row.get::<i32, _>("completed_steps") as u64,
                    metadata: GoalMetadata {
                        photos_taken: photos.iter().map(ToString::to_string).collect(),
                    },
                },
            });
        }
        let status = if is_paused {
            "ERR_PROGRAM_PAUSED".into()
        } else {
            season_state(&mut tx, &s, week).await?
        };
        (
            ProgressSeason {
                time_left: (s.end_date - now).num_seconds().max(0),
                start_date: iso(s.start_date),
                end_date: iso(s.end_date),
                season_state: status,
            },
            ProgressWeek {
                time_left: (end - now).num_seconds().max(0),
            },
        )
    } else {
        let last: Option<(NaiveDateTime,NaiveDateTime)> = sqlx::query_as("SELECT start_date,end_date FROM credits.seasons WHERE end_date<$1 ORDER BY end_date DESC LIMIT 1").bind(now).fetch_optional(&mut *tx).await?;
        let (start, end) = last.unwrap_or((now, now));
        (
            ProgressSeason {
                time_left: 0,
                start_date: iso(start),
                end_date: iso(end),
                season_state: "ENDED".into(),
            },
            ProgressWeek { time_left: 0 },
        )
    };
    let captcha_blocked = super::super::captcha::cooldown(&mut tx, address, now).await? > 0;
    tx.commit().await?;
    Ok(CreditsProgramProgressResponse {
        season,
        current_week,
        user: UserData {
            has_started_program: has_started && !opted,
        },
        credits: CreditsData {
            available: total / 1e18,
            expires_in: expires,
            is_blocked_for_claiming: wallet.is_some_and(|w| w.is_blocked_for_claiming)
                || captcha_blocked,
        },
        goals,
    })
}

pub async fn season_document(state: &SharedState) -> Result<SeasonsData, ApiError> {
    let is_paused = paused(state).await;
    let now = super::super::store::now();
    let mut conn = state.credits.pool.acquire().await?;
    let rows: Vec<Season> = sqlx::query_as("(SELECT id,name,start_date,end_date,max_mana_wei::text,grace_days FROM credits.seasons WHERE end_date<$1 ORDER BY end_date DESC LIMIT 1) UNION ALL (SELECT id,name,start_date,end_date,max_mana_wei::text,grace_days FROM credits.seasons WHERE start_date<=$1 AND end_date>=$1 ORDER BY start_date DESC LIMIT 1) UNION ALL (SELECT id,name,start_date,end_date,max_mana_wei::text,grace_days FROM credits.seasons WHERE start_date>$1 ORDER BY start_date LIMIT 1)").bind(now).fetch_all(&mut *conn).await?;
    let mut data = SeasonsData {
        last_season: None,
        current_season: None,
        next_season: None,
    };
    for s in rows {
        let (number, start, end) = s.week(now);
        let active = s.start_date <= now && s.end_date >= now;
        let wire = SeasonData {
            id: s.id,
            name: s.name.clone(),
            start_date: iso(s.start_date),
            end_date: iso(s.end_date),
            max_mana: s.max_mana_wei.clone(),
            time_left: (s.end_date - now).num_seconds().max(0),
            amount_of_weeks: ((s.end_date - s.start_date).num_seconds() as f64 / 604800.0).ceil()
                as i32,
            state: if active {
                if is_paused {
                    "ERR_PROGRAM_PAUSED".into()
                } else {
                    season_state(&mut conn, &s, number).await?
                }
            } else {
                "ENDED".into()
            },
        };
        if active {
            data.current_season = Some(CurrentSeasonInfo {
                season: wire,
                week: Week {
                    week_number: number,
                    time_left: (end - now).num_seconds().max(0) as u64,
                    start_date: iso(start),
                    end_date: iso(end),
                    seconds_remaining: (end - now).num_seconds().max(0) as u64,
                },
            });
        } else {
            let summary = SeasonSummary {
                id: wire.id,
                name: wire.name,
                start_date: wire.start_date,
                end_date: wire.end_date,
                max_mana: wire.max_mana,
            };
            if s.end_date < now {
                data.last_season = Some(summary);
            } else {
                data.next_season = Some(summary);
            }
        }
    }
    Ok(data)
}

pub async fn season_status(state: &SharedState) -> Result<Value, ApiError> {
    let is_paused = paused(state).await;
    let mut conn = state.credits.pool.acquire().await?;
    let now = super::super::store::now();
    let season = current(&mut conn, now)
        .await?
        .ok_or_else(|| ApiError::not_found("No active season found"))?;
    Ok(json!({"id":season.id,"week":season.week(now).0,"isPaused":is_paused}))
}

pub async fn flag_reason(
    conn: &mut PgConnection,
    address: &str,
) -> Result<Option<(String, CreditFlagReason)>, ApiError> {
    let row = sqlx::query(
        "SELECT source,ip,linked_addresses,flagged_at FROM credits.wallet_flags WHERE address=$1",
    )
    .bind(address)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|row| {
        let source: String = row.get("source");
        let message = match source.as_str() {
            "ip_multi_accounting" => "Cloudflare IP - Multi-accounting detected",
            "web_anon_multi_accounting" => "Web Anon - Multi-accounting detected",
            "explorer_anon_multi_accounting" => "Explorer Anon - Multi-accounting detected",
            "web_anon_denylisted_address" => "Web Anon - Denylisted address detected",
            "explorer_anon_denylisted_address" => "Explorer Anon - Denylisted address detected",
            "web_ip_multi_accounting" => "Web IP - Multi-accounting detected",
            "explorer_ip_multi_accounting" => "Explorer IP - Multi-accounting detected",
            _ => "Multi-accounting detected",
        };
        let at = row
            .get::<NaiveDateTime, _>("flagged_at")
            .format("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)")
            .to_string();
        (
            at,
            CreditFlagReason {
                message: message.into(),
                ip: row.get("ip"),
                linked_addresses: row
                    .get::<Vec<String>, _>("linked_addresses")
                    .into_iter()
                    .filter(|a| a != address)
                    .collect(),
            },
        )
    }))
}

async fn blocked_error(conn: &mut PgConnection, address: &str) -> Result<ApiError, ApiError> {
    Ok(
        if let Some((at, reason)) = flag_reason(conn, address).await? {
            ApiError::ClaimBlocked {
                message: format!("User {address} was flagged at {at}"),
                reason: Some(reason),
            }
        } else {
            ApiError::ClaimBlocked {
                message: "Wallet is blocked for claiming".into(),
                reason: None,
            }
        },
    )
}

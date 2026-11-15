//! Community federation mutations.

use super::*;

pub(super) async fn create(ctx: &Ctx<'_>) -> Response {
    let (signed, signer) = match preflight::<CommunityCreate>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    for check in [
        validate::validate_name(&m.name),
        validate::check_restricted_name(&m.name, &ctx.state.settings.restricted_names),
        validate::validate_description(&m.description),
    ] {
        if let Err(e) = check {
            return bad(e);
        }
    }
    if !match ctx.state.has_owned_name(&signer).await {
        Ok(owns) => owns,
        Err(error) => return error.into_response(),
    } {
        return err_json(
            StatusCode::UNAUTHORIZED,
            format!("The user {signer} doesn't have any names"),
        );
    }
    // The REST create's content check: a signed envelope is not a way around it.
    if let Err(error) = ctx
        .state
        .validate_content(super::super::compliance::Content {
            name: Some(&m.name),
            description: Some(&m.description),
            thumbnail: None,
        })
        .await
    {
        return error.into_response();
    }
    let community_hex = community_id_hex(signer.as_str(), &m.name, &signed.nonce);
    let community = CommunityId::from_federation_hex(&community_hex);
    let signature_hash = signed.signature_hash();
    let applied: sqlx::Result<bool> = async {
        let mut tx = ctx.state.pool.begin().await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM social.communities WHERE id = $1)").bind(community).fetch_one(&mut *tx).await?;
        if exists {
            return Ok(false);
        }
        sqlx::query("INSERT INTO social.communities (id, name, description, owner_address, private, unlisted) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(community)
            .bind(&m.name)
            .bind(&m.description)
            .bind(&signer)
            .bind(m.private)
            .bind(m.unlisted)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO social.community_members (community_id, member_address, role) VALUES ($1, $2, 'owner') ON CONFLICT (community_id, member_address) DO NOTHING")
            .bind(community)
            .bind(&signer)
            .execute(&mut *tx)
            .await?;
        record(&mut tx, &signature_hash, &community_hex, community, &signer, signed.signed_at, &payload(m), &Entry { kind: "create", target: Some(signer.to_string()), role: Some("owner"), ..Default::default() }).await?;
        tx.commit().await?;
        Ok(true)
    }
    .await;
    match applied {
        Ok(true) => ok_with(
            signature_hash,
            json!({ "community_id": community_hex, "id": community.as_uuid(), "expected_id": community_hex }),
        ),
        Ok(false) => err_json(StatusCode::CONFLICT, "community already exists"),
        Err(e) => db_err(e),
    }
}

pub(super) async fn update(ctx: &Ctx<'_>, id: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityUpdate>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let community = match path_community(id, &m.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    for check in [
        validate::validate_name_opt(m.name.as_deref()),
        validate::check_restricted_name_opt(
            m.name.as_deref(),
            &ctx.state.settings.restricted_names,
        ),
        validate::validate_description_opt(m.description.as_deref()),
    ] {
        if let Err(e) = check {
            return bad(e);
        }
    }
    if let Err(r) = require_permission(
        ctx.state,
        community,
        &signer,
        CommunityPermission::EditInfo,
        "edit the community",
    )
    .await
    {
        return *r;
    }
    if m.name.is_some()
        && let Err(r) = require_permission(
            ctx.state,
            community,
            &signer,
            CommunityPermission::EditName,
            "edit the community name",
        )
        .await
    {
        return *r;
    }
    if (m.private.is_some() || m.unlisted.is_some())
        && let Err(r) = require_permission(
            ctx.state,
            community,
            &signer,
            CommunityPermission::EditSettings,
            "update the community privacy",
        )
        .await
    {
        return *r;
    }
    // The REST update's content check, gated the same way: only text that
    // changes beyond case and surrounding whitespace is classified.
    if m.name.is_some() || m.description.is_some() {
        let current: Option<(String, String)> =
            match sqlx::query_as("SELECT name, description FROM social.communities WHERE id = $1")
                .bind(community)
                .fetch_optional(&ctx.state.pool)
                .await
            {
                Ok(current) => current,
                Err(e) => return db_err(e),
            };
        let changed = |new: Option<&str>, old: fn(&(String, String)) -> &str| {
            new.is_some_and(|new| {
                current
                    .as_ref()
                    .is_some_and(|c| super::super::compliance::changed(new, old(c)))
            })
        };
        if (changed(m.name.as_deref(), |c| &c.0) || changed(m.description.as_deref(), |c| &c.1))
            && let Err(error) = ctx
                .state
                .validate_content(super::super::compliance::Content {
                    name: m.name.as_deref(),
                    description: m.description.as_deref(),
                    thumbnail: None,
                })
                .await
        {
            return error.into_response();
        }
    }
    let signature_hash = signed.signature_hash();
    let applied: sqlx::Result<()> = async {
        let mut tx = ctx.state.pool.begin().await?;
        sqlx::query(
            "UPDATE social.communities SET name = COALESCE($2, name), description = COALESCE($3, description), \
               private = COALESCE($4, private), unlisted = COALESCE($5, unlisted), updated_at = timezone('UTC', now()) WHERE id = $1",
        )
        .bind(community)
        .bind(m.name.as_deref())
        .bind(m.description.as_deref())
        .bind(m.private)
        .bind(m.unlisted)
        .execute(&mut *tx)
        .await?;
        record(&mut tx, &signature_hash, &m.community_id, community, &signer, signed.signed_at, &payload(m), &Entry { kind: "update", ..Default::default() }).await?;
        tx.commit().await
    }
    .await;
    applied.map(|_| ok(signature_hash)).unwrap_or_else(db_err)
}

pub(super) async fn delete(ctx: &Ctx<'_>, id: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityDelete>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let community = match path_community(id, &signed.message.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if let Err(r) = require_at_least(ctx.state, community, &signer, CommunityStanding::Owner).await
    {
        return *r;
    }
    let signature_hash = signed.signature_hash();
    let applied: sqlx::Result<bool> = async {
        let mut tx = ctx.state.pool.begin().await?;
        sqlx::query("UPDATE social.communities SET active = FALSE, updated_at = timezone('UTC', now()) WHERE id = $1").bind(community).execute(&mut *tx).await?;
        let fresh = record(&mut tx, &signature_hash, &signed.message.community_id, community, &signer, signed.signed_at, &payload(&signed.message), &Entry { kind: "delete", ..Default::default() }).await?;
        tx.commit().await?;
        Ok(fresh)
    }
    .await;
    if let Ok(true) = applied {
        ctx.state.community_closed_to_worlds(community);
    }
    applied.map(|_| ok(signature_hash)).unwrap_or_else(db_err)
}

/// Logs a role action and projects it into the member row unless a newer
/// one on the same member was logged first. `None` refuses a grant to a
/// banned member: under the membership lock the REST writers take, a ban
/// committed after the handler's pre-check is still seen.
#[allow(clippy::too_many_arguments)]
async fn apply_role_action<T: TypedMessage>(
    state: &CommunitiesState,
    signed: &Signed<T>,
    community_hex: &str,
    community: CommunityId,
    signer: &Address,
    member: &Address,
    role: &'static str,
    extra: impl AsyncFnOnce(&mut PgConnection) -> sqlx::Result<()>,
) -> sqlx::Result<Option<String>> {
    let signature_hash = signed.signature_hash();
    let mut removed = false;
    let grants = matches!(role, "member" | "mod" | "owner");
    let mut guard = super::super::write_support::acquire_membership(&state.pool, community).await?;
    let mut tx = guard.begin().await?;
    if grants
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM social.community_bans \
               WHERE community_id = $1 AND banned_address = $2 AND active)",
        )
        .bind(community)
        .bind(member)
        .fetch_one(&mut *tx)
        .await?
    {
        return Ok(None);
    }
    if T::PRIMARY_TYPE == "CommunityJoin"
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM social.community_members WHERE community_id=$1 AND member_address=$2)",
        )
        .bind(community)
        .bind(member)
        .fetch_one(&mut *tx)
        .await?
    {
        // Joining again preserves the existing role. Do not let this no-op
        // create a newer role action that can suppress a real role update.
        // Preflight already consumed the nonce, so replay still fails.
        tx.commit().await?;
        return Ok(Some(signature_hash));
    }
    let fresh = record(
        &mut tx,
        &signature_hash,
        community_hex,
        community,
        signer,
        signed.signed_at,
        &payload(&signed.message),
        &Entry {
            kind: "role",
            target: Some(member.to_string()),
            role: Some(role),
            ..Default::default()
        },
    )
    .await?;
    if fresh
        && !superseded(
            &mut tx,
            community,
            member.as_str(),
            signed.signed_at,
            &signature_hash,
            matches!(T::PRIMARY_TYPE, "CommunityBan" | "CommunityUnban"),
        )
        .await?
    {
        match role {
            _ if T::PRIMARY_TYPE == "CommunityUnban" => {
                // Lifting a ban never revokes membership. Another moderator
                // may already have unbanned this wallet and allowed it to rejoin.
            }
            _ if grants => {
                let stored = CommunityRole::parse(role)
                    .map(CommunityRole::as_stored_text)
                    .unwrap_or("member");
                let conflict = if T::PRIMARY_TYPE == "CommunityJoin" {
                    "DO NOTHING"
                } else {
                    "DO UPDATE SET role = EXCLUDED.role"
                };
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "INSERT INTO social.community_members (community_id, member_address, role) VALUES ($1, $2, $3) ON CONFLICT (community_id, member_address) {conflict}"
                )))
                .bind(community)
                .bind(member)
                .bind(stored)
                .execute(&mut *tx)
                .await?;
            }
            _ => {
                removed = sqlx::query("DELETE FROM social.community_members WHERE community_id = $1 AND member_address = $2").bind(community).bind(member).execute(&mut *tx).await?.rows_affected() > 0;
            }
        }
        extra(&mut tx).await?;
    }
    tx.commit().await?;
    drop(guard);
    if removed {
        state.member_removed_from_worlds(community, member);
    }
    Ok(Some(signature_hash))
}

/// A role action that grants membership answers 403 when its member is banned.
fn role_action_reply(result: sqlx::Result<Option<String>>) -> Response {
    match result {
        Ok(Some(signature_hash)) => ok(signature_hash),
        Ok(None) => err_json(StatusCode::FORBIDDEN, "banned from community"),
        Err(e) => db_err(e),
    }
}

pub(super) async fn join(ctx: &Ctx<'_>, id: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityJoin>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let community = match path_community(id, &signed.message.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match super::super::queries::community_exists(&ctx.state.pool, community, false).await {
        Ok(true) => {}
        Ok(false) => return err_json(StatusCode::NOT_FOUND, "community not found"),
        Err(e) => return db_err(e),
    }
    match super::super::queries::is_private(&ctx.state.pool, community).await {
        Ok(true) => {
            return err_json(
                StatusCode::UNAUTHORIZED,
                format!(
                    "Cannot join private community {id} directly; a join request or invite is required"
                ),
            );
        }
        Ok(false) => {}
        Err(e) => return db_err(e),
    }
    match is_banned(ctx.state, community, &signer).await {
        Ok(true) => return err_json(StatusCode::FORBIDDEN, "banned from community"),
        Ok(false) => {}
        Err(r) => return *r,
    }
    role_action_reply(
        apply_role_action(
            ctx.state,
            &signed,
            &signed.message.community_id,
            community,
            &signer,
            &signer,
            "member",
            async |_| Ok(()),
        )
        .await,
    )
}

pub(super) async fn leave(ctx: &Ctx<'_>, id: &str, address: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityLeave>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let community = match path_community(id, &signed.message.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if !signed.message.member.eq_ignore_ascii_case(signer.as_str()) {
        return err_json(StatusCode::FORBIDDEN, "may only leave on behalf of self");
    }
    if !address.eq_ignore_ascii_case(signer.as_str()) {
        return err_json(StatusCode::FORBIDDEN, "path address must match signer");
    }
    match load_standing(&ctx.state.pool, community, &signer).await {
        Ok(s) if s.tier() == CommunityStanding::Owner => {
            return err_json(
                StatusCode::UNAUTHORIZED,
                format!("The owner cannot leave the community {id}"),
            );
        }
        Ok(_) => {}
        Err(refusal) => return own_detail(&refusal),
    }
    role_action_reply(
        apply_role_action(
            ctx.state,
            &signed,
            &signed.message.community_id,
            community,
            &signer,
            &signer,
            "none",
            async |_| Ok(()),
        )
        .await,
    )
}

pub(super) async fn role(ctx: &Ctx<'_>, id: &str, address: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityRoleUpdate>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let community = match path_community(id, &m.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if !m.target.eq_ignore_ascii_case(address) {
        return bad("target must match path address");
    }
    let role: &'static str = match CommunityStanding::parse_supplied(&m.role) {
        Some(CommunityStanding::Member) => "member",
        Some(CommunityStanding::Moderator) => "mod",
        _ => return bad("invalid role"),
    };
    if m.target.eq_ignore_ascii_case(signer.as_str()) {
        return err_json(StatusCode::FORBIDDEN, "a user cannot update their own role");
    }
    let Ok(target) = Address::parse(&m.target) else {
        return bad("target must match path address");
    };
    let (actor_standing, target_standing) =
        match load_standing_pair(&ctx.state.pool, community, &signer, &target).await {
            Ok(pair) => pair,
            Err(refusal) => return own_detail(&refusal),
        };
    if !actor_standing.has(CommunityPermission::AssignRoles)
        || !can_act_on_member(actor_standing.tier(), target_standing.tier())
    {
        return err_json(
            StatusCode::FORBIDDEN,
            "actor cannot assign roles for this member",
        );
    }
    role_action_reply(
        apply_role_action(
            ctx.state,
            &signed,
            &m.community_id,
            community,
            &signer,
            &target,
            role,
            async |_| Ok(()),
        )
        .await,
    )
}

pub(super) async fn ban(ctx: &Ctx<'_>, id: &str, address: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityBan>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let community = match path_community(id, &m.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if !m.target.eq_ignore_ascii_case(address) {
        return bad("target must match path address");
    }
    let Ok(target) = Address::parse(&m.target) else {
        return bad("target must match path address");
    };
    if let Err(refusal) =
        CommunityBanAuthority::resolve(&ctx.state.pool, &signer, community, &target).await
    {
        return own_detail(&refusal);
    }
    let (reason, banned_by) = (m.reason.clone(), signer.clone());
    let banned = target.clone();
    let result = apply_role_action(ctx.state, &signed, &m.community_id, community, &signer, &target, "banned", async move |tx: &mut PgConnection| {
        sqlx::query("DELETE FROM social.community_requests WHERE community_id = $1 AND member_address = $2 AND status = 'pending'").bind(community).bind(&banned).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO social.community_bans (community_id, banned_address, banned_by, reason, active, banned_at) \
             VALUES ($1, $2, $3, $4, TRUE, timezone('UTC', now())) \
             ON CONFLICT (community_id, banned_address) DO UPDATE \
               SET active = TRUE, banned_by = EXCLUDED.banned_by, reason = EXCLUDED.reason, banned_at = timezone('UTC', now()), \
                   unbanned_by = NULL, unbanned_at = NULL",
        )
        .bind(community)
        .bind(&banned)
        .bind(&banned_by)
        .bind(reason.as_deref())
        .execute(&mut *tx)
        .await?;
        Ok(())
    })
    .await;
    match result {
        Ok(Some(sig)) => {
            ctx.state
                .evict_from_private_community_voice(community, &target)
                .await;
            ok(sig)
        }
        other => role_action_reply(other),
    }
}

pub(super) async fn unban(ctx: &Ctx<'_>, id: &str, address: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityUnban>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let community = match path_community(id, &m.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if !m.target.eq_ignore_ascii_case(address) {
        return bad("target must match path address");
    }
    let Ok(target) = Address::parse(&m.target) else {
        return bad("target must match path address");
    };
    if let Err(refusal) =
        CommunityUnbanAuthority::resolve(&ctx.state.pool, &signer, community, &target).await
    {
        return own_detail(&refusal);
    }
    let (lifted, unbanned_by) = (target.clone(), signer.clone());
    role_action_reply(apply_role_action(ctx.state, &signed, &m.community_id, community, &signer, &target, "none", async move |tx: &mut PgConnection| {
        sqlx::query(
            "UPDATE social.community_bans SET active = FALSE, unbanned_by = $3, unbanned_at = timezone('UTC', now()) \
             WHERE community_id = $1 AND banned_address = $2 AND active",
        )
        .bind(community)
        .bind(&lifted)
        .bind(&unbanned_by)
        .execute(&mut *tx)
        .await?;
        Ok(())
    })
    .await,
    )
}

pub(super) async fn places_add(ctx: &Ctx<'_>, id: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityPlacesAdd>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let community = match path_community(id, &m.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if let Err(r) = require_permission(
        ctx.state,
        community,
        &signer,
        CommunityPermission::AddPlaces,
        "add places to the community",
    )
    .await
    {
        return *r;
    }
    if let Err(e) = ctx
        .state
        .validate_places_ownership(&m.place_ids, &signer)
        .await
    {
        return e.into_response();
    }
    let signature_hash = signed.signature_hash();
    let applied: sqlx::Result<()> = async {
        let mut tx = ctx.state.pool.begin().await?;
        for (i, place) in m.place_ids.iter().enumerate() {
            let entry = Entry { kind: "place", target: Some(place.clone()), action: Some("add".into()), ..Default::default() };
            record(&mut tx, &format!("{signature_hash}-add-{i}"), &m.community_id, community, &signer, signed.signed_at, &json!({ "place_id": place, "action": "add" }), &entry).await?;
        }
        sqlx::query(
            "INSERT INTO social.community_places (place_id, community_id, added_by) SELECT t.pid, $1, $2 FROM unnest($3::text[]) AS t(pid) \
             ON CONFLICT (place_id, community_id) DO NOTHING",
        )
        .bind(community)
        .bind(&signer)
        .bind(&m.place_ids)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }
    .await;
    applied.map(|_| ok(signature_hash)).unwrap_or_else(db_err)
}

pub(super) async fn place_remove(ctx: &Ctx<'_>, id: &str, place: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityPlaceRemove>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let community = match path_community(id, &m.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if m.place_id != place {
        return bad("place_id mismatch");
    }
    if let Err(r) = require_permission(
        ctx.state,
        community,
        &signer,
        CommunityPermission::RemovePlaces,
        "remove places from the community",
    )
    .await
    {
        return *r;
    }
    if let Err(e) = ctx
        .state
        .validate_places_ownership(std::slice::from_ref(&m.place_id), &signer)
        .await
    {
        return e.into_response();
    }
    let signature_hash = signed.signature_hash();
    let applied: sqlx::Result<()> = async {
        let mut tx = ctx.state.pool.begin().await?;
        let entry = Entry {
            kind: "place",
            target: Some(m.place_id.clone()),
            action: Some("remove".into()),
            ..Default::default()
        };
        record(
            &mut tx,
            &signature_hash,
            &m.community_id,
            community,
            &signer,
            signed.signed_at,
            &payload(m),
            &entry,
        )
        .await?;
        sqlx::query(
            "DELETE FROM social.community_places WHERE place_id = $1 AND community_id = $2",
        )
        .bind(&m.place_id)
        .bind(community)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }
    .await;
    applied.map(|_| ok(signature_hash)).unwrap_or_else(db_err)
}

pub(super) fn is_valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) async fn post(ctx: &Ctx<'_>, id: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityPost>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let community = match path_community(id, &m.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if let Err(r) = require_permission(
        ctx.state,
        community,
        &signer,
        CommunityPermission::CreatePosts,
        "create posts in the community",
    )
    .await
    {
        return *r;
    }
    // The logged hash must equal the signed one, and bodies are keyed lowercase.
    if m.content_hash.bytes().any(|c| c.is_ascii_uppercase()) {
        return bad("content_hash must be lowercase");
    }
    let hash = m.content_hash.clone();
    let signature_hash = signed.signature_hash();
    // Under the uploads lock, so an upload or the collector racing this post
    // sees it, and it sees them.
    let applied: sqlx::Result<Result<bool, Response>> = async {
        let mut tx = ctx.state.pool.begin().await?;
        lock_uploads(&mut tx).await?;
        let body = uploaded_body(ctx.state, &mut tx, &hash, &signer).await?;
        let text = match body.as_deref().map(std::str::from_utf8) {
            None => {
                tracing::debug!(community_id = %m.community_id, content_hash = %hash, %signer, "CommunityPost accepted before its content body is here");
                hash.clone()
            }
            Some(Ok(raw)) => match super::super::posts::validate_text(raw) {
                Ok(text) => text,
                Err(refused) => return Ok(Err(refused)),
            },
            Some(Err(_)) => return Ok(Err(bad("content body is not valid UTF-8"))),
        };
        let entry = Entry { kind: "post", content_hash: Some(hash.clone()), ..Default::default() };
        if record(&mut tx, &signature_hash, &m.community_id, community, &signer, signed.signed_at, &payload(m), &entry).await? {
            sqlx::query("INSERT INTO social.community_posts (id, community_id, author_address, content) VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING")
                .bind(post_uuid(&signature_hash))
                .bind(community)
                .bind(&signer)
                .bind(&text)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE social.community_content_uploads SET posted_at = timezone('UTC', now()) WHERE content_hash = $1 AND uploader = $2 AND posted_at IS NULL")
                .bind(&hash)
                .bind(&signer)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(Ok(body.is_some()))
    }
    .await;
    match applied {
        Ok(Ok(local)) => ok_with(signature_hash, json!({ "content_body_local": local })),
        Ok(Err(refused)) => refused,
        Err(e) => upload_lock_err(e),
    }
}

/// `(community hex id, author)` of a logged post.
async fn post_meta(
    pool: &PgPool,
    post_signature_hash: &str,
) -> sqlx::Result<Option<(String, String)>> {
    sqlx::query_as("SELECT community_id, signer FROM social.community_signed_actions WHERE signature_hash = $1 AND kind = 'post'").bind(post_signature_hash).fetch_optional(pool).await
}

pub(super) async fn post_delete(ctx: &Ctx<'_>, id: &str, post: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityPostDelete>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let meta = match post_meta(&ctx.state.pool, &m.post_id).await {
        Ok(meta) => meta,
        Err(e) => return db_err(e),
    };
    let (post_community, author) = meta.map(|(c, a)| (c, Some(a))).unwrap_or_default();
    if m.community_id != post_community || m.post_id != post {
        return bad("post_id / community_id mismatch");
    }
    let community = CommunityId::from_federation_hex(&m.community_id);
    let is_author = author
        .as_deref()
        .is_some_and(|a| a.eq_ignore_ascii_case(signer.as_str()));
    let standing = match load_standing(&ctx.state.pool, community, &signer).await {
        Ok(s) => s,
        Err(refusal) => return own_detail(&refusal),
    };
    if !can_delete_post(standing.tier(), is_author) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            format!("The user {signer} doesn't have permission to delete posts from the community"),
        );
    }
    let signature_hash = signed.signature_hash();
    let applied: sqlx::Result<()> = async {
        let mut tx = ctx.state.pool.begin().await?;
        record(
            &mut tx,
            &signature_hash,
            &m.community_id,
            community,
            &signer,
            signed.signed_at,
            &payload(m),
            &Entry {
                kind: "post_delete",
                target: Some(m.post_id.clone()),
                ..Default::default()
            },
        )
        .await?;
        sqlx::query("DELETE FROM social.community_posts WHERE id = $1 AND community_id = $2")
            .bind(post_uuid(&m.post_id))
            .bind(community)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    .await;
    applied.map(|_| ok(signature_hash)).unwrap_or_else(db_err)
}

pub(super) async fn like(ctx: &Ctx<'_>, id: &str, post: &str, liking: bool) -> Response {
    let community = match Uuid::parse_str(id) {
        Ok(u) => CommunityId::from_uuid(u),
        Err(_) => return bad("invalid uuid"),
    };
    let (community_hex, post_id, signature_hash, signed_at, message, signer) = if liking {
        let (signed, signer) = match preflight::<CommunityPostLike>(ctx).await {
            Ok(x) => x,
            Err(r) => return *r,
        };
        let m = signed.message.clone();
        (
            m.community_id.clone(),
            m.post_id.clone(),
            signed.signature_hash(),
            signed.signed_at,
            payload(&m),
            signer,
        )
    } else {
        let (signed, signer) = match preflight::<CommunityPostUnlike>(ctx).await {
            Ok(x) => x,
            Err(r) => return *r,
        };
        let m = signed.message.clone();
        (
            m.community_id.clone(),
            m.post_id.clone(),
            signed.signature_hash(),
            signed.signed_at,
            payload(&m),
            signer,
        )
    };
    if CommunityId::from_federation_hex(&community_hex) != community {
        return bad("community_id mismatch");
    }
    if post_id != post {
        return bad("post_id mismatch");
    }
    let private =
        match sqlx::query_scalar::<_, bool>("SELECT private FROM social.communities WHERE id = $1")
            .bind(community)
            .fetch_optional(&ctx.state.pool)
            .await
        {
            Ok(Some(p)) => p,
            Ok(None) => return err_json(StatusCode::NOT_FOUND, "community not found"),
            Err(e) => return db_err(e),
        };
    let standing = match load_standing(&ctx.state.pool, community, &signer).await {
        Ok(s) => s,
        Err(refusal) => return own_detail(&refusal),
    };
    let tier = match is_banned(ctx.state, community, &signer).await {
        Ok(true) => CommunityStanding::Banned,
        Ok(false) => standing.tier(),
        Err(r) => return *r,
    };
    if !can_like_post(tier, private) {
        return err_json(
            StatusCode::UNAUTHORIZED,
            format!("{signer} cannot like/unlike posts in community {community_hex}"),
        );
    }
    match post_meta(&ctx.state.pool, &post_id).await {
        Ok(Some((c, _))) if c == community_hex => {}
        Ok(_) => {
            return err_json(
                StatusCode::NOT_FOUND,
                "post does not belong to this community",
            );
        }
        Err(e) => return db_err(e),
    }
    let kind = if liking { "post_like" } else { "post_unlike" };
    let applied: sqlx::Result<()> = async {
        let mut tx = ctx.state.pool.begin().await?;
        record(&mut tx, &signature_hash, &community_hex, community, &signer, signed_at, &message, &Entry { kind, target: Some(post_id.clone()), ..Default::default() }).await?;
        let sql = if liking {
            "INSERT INTO social.community_post_likes (post_id, user_address) SELECT id, $2 FROM social.community_posts WHERE id = $1 ON CONFLICT (post_id, user_address) DO NOTHING"
        } else {
            "DELETE FROM social.community_post_likes WHERE post_id = $1 AND user_address = $2"
        };
        sqlx::query(sql).bind(post_uuid(&post_id)).bind(&signer).execute(&mut *tx).await?;
        tx.commit().await
    }
    .await;
    applied.map(|_| ok(signature_hash)).unwrap_or_else(db_err)
}

pub(super) async fn request_status(ctx: &Ctx<'_>, id: &str, request: &str) -> Response {
    if let Err(r) = parse_uuid(id) {
        return *r;
    }
    let (signed, signer) = match preflight::<CommunityRequestStatusUpdate>(ctx).await {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let m = &signed.message;
    let community = match path_community(id, &m.community_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    if m.request_id != request {
        return bad("request_id mismatch");
    }
    let status = match m.status.as_str() {
        s @ ("accepted" | "rejected" | "cancelled") => s,
        _ => return bad("invalid request status"),
    };
    let request_uuid = Uuid::parse_str(&m.request_id).ok();
    let row: Option<(Uuid, Address, String, String)> = match request_uuid {
        Some(id) => match sqlx::query_as(
            "SELECT community_id, member_address, type, status FROM social.community_requests WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&ctx.state.pool)
        .await
        {
            Ok(row) => row,
            Err(e) => return db_err(e),
        },
        None => None,
    };
    if row
        .as_ref()
        .is_some_and(|(c, ..)| *c != community.as_uuid())
    {
        return err_json(
            StatusCode::NOT_FOUND,
            "request does not belong to this community",
        );
    }
    // A pending request is settled as the client route settles it; anything
    // else is only logged, by a moderator.
    let pending = row
        .filter(|(.., state)| state.as_str() == "pending")
        .zip(request_uuid)
        .map(|((_, member, kind, _), id)| (id, member, kind));
    let refused = match &pending {
        Some((_, member, kind)) => {
            let banned = if *member == signer {
                Ok(false)
            } else {
                is_banned(ctx.state, community, &signer).await
            };
            match banned {
                Ok(true) => Some(err_json(
                    StatusCode::FORBIDDEN,
                    "Forbidden: banned from this community",
                )),
                Ok(false) => {
                    super::super::requests::request_authority(
                        ctx.state, community, &signer, member, kind, status,
                    )
                    .await
                }
                Err(r) => Some(*r),
            }
        }
        None => require_at_least(ctx.state, community, &signer, CommunityStanding::Moderator)
            .await
            .err()
            .map(|r| *r),
    };
    if let Some(refused) = refused {
        return refused;
    }
    let signature_hash = signed.signature_hash();
    let applied: sqlx::Result<Result<bool, Response>> = async {
        let mut guard =
            super::super::write_support::acquire_membership(&ctx.state.pool, community).await?;
        let mut tx = guard.begin().await?;
        let entry = Entry {
            kind: "request_status",
            target: Some(m.request_id.clone()),
            action: Some(status.to_string()),
            ..Default::default()
        };
        let fresh = record(
            &mut tx,
            &signature_hash,
            &m.community_id,
            community,
            &signer,
            signed.signed_at,
            &payload(m),
            &entry,
        )
        .await?;
        let mut settled = false;
        if let Some((request_uuid, member, _)) = pending.as_ref().filter(|_| fresh) {
            if status == "accepted" && is_banned_on(&mut tx, community, member).await? {
                return Ok(Err(err_json(
                    StatusCode::UNAUTHORIZED,
                    format!("The user {member} is banned from the community {community}"),
                )));
            }
            // Settled meanwhile by the client route: the entry is still logged.
            match super::super::requests::transition_request(
                &mut tx,
                community,
                *request_uuid,
                member,
                status,
            )
            .await
            {
                Ok(()) => settled = true,
                Err(refused) if refused.status() == StatusCode::NOT_FOUND => {}
                Err(refused) => return Ok(Err(*refused)),
            }
        }
        tx.commit().await?;
        drop(guard);
        Ok(Ok(settled))
    }
    .await;
    match applied {
        Ok(Ok(settled)) => {
            if let Some((request_uuid, member, kind)) = pending.as_ref().filter(|_| settled) {
                super::super::requests::request_settled(
                    ctx.state,
                    community,
                    (*request_uuid, &m.request_id),
                    member,
                    kind,
                    status,
                    &signer,
                )
                .await;
            }
            ok(signature_hash)
        }
        Ok(Err(refused)) => refused,
        Err(e) => db_err(e),
    }
}

async fn is_banned_on(
    conn: &mut PgConnection,
    community: CommunityId,
    member: &Address,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM social.community_bans WHERE community_id = $1 AND banned_address = $2 AND active)",
    )
    .bind(community)
    .bind(member)
    .fetch_one(conn)
    .await
}

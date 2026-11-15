//! The friendship facts the community routes decorate with: the wire
//! `friendshipStatus` enum of a member row relative to the viewer, and the
//! viewer's friends inside a batch of communities. Reads
//! `social.friendships` / `social.friendship_actions` over the ONE pool (the
//! old service reached them through a second `MUTES_PG_CONNECTION_STRING`
//! pool). Community HTTP reads propagate these core query failures; the legacy
//! best-effort status wrapper remains available to its existing consumers.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::{Address, CommunityId};

pub mod friendship_status {
    pub const REQUEST_SENT: i32 = 0;
    pub const REQUEST_RECEIVED: i32 = 1;
    pub const CANCELED: i32 = 2;
    pub const ACCEPTED: i32 = 3;
    pub const REJECTED: i32 = 4;
    pub const DELETED: i32 = 5;
    pub const BLOCKED: i32 = 6;
    pub const NONE: i32 = 7;
    pub const BLOCKED_BY: i32 = 8;

    pub const UNRECOGNIZED: i32 = -1;
}

pub fn friendship_request_status(action: &str, acting_user: &str, context_address: &str) -> i32 {
    use friendship_status::*;
    let acting_is_context = acting_user.eq_ignore_ascii_case(context_address);
    match action {
        "accept" => ACCEPTED,
        "cancel" => CANCELED,
        "delete" => DELETED,
        "reject" => REJECTED,
        "request" => {
            if acting_is_context {
                REQUEST_SENT
            } else {
                REQUEST_RECEIVED
            }
        }
        "block" => {
            if acting_is_context {
                BLOCKED
            } else {
                BLOCKED_BY
            }
        }
        _ => UNRECOGNIZED,
    }
}

/// The latest action of the viewer's friendship with each address, as the
/// wire status; addresses with no friendship row are absent (NONE).
pub async fn try_friendship_statuses(
    pool: &PgPool,
    viewer: &Address,
    addresses: &[String],
) -> sqlx::Result<HashMap<String, i32>> {
    Ok(try_friendship_actions(pool, viewer, addresses)
        .await?
        .into_iter()
        .map(|(address, action)| (address, action.status(viewer)))
        .collect())
}

pub(crate) struct FriendshipAction {
    pub action: String,
    pub acting_user: String,
}

impl FriendshipAction {
    pub fn status(&self, viewer: &Address) -> i32 {
        friendship_request_status(&self.action, &self.acting_user, viewer.as_str())
    }
}

/// Preserve the SQL action/actor aliases for signed V1 members and bans.
/// V2 uses the same facts only to calculate its numeric status.
pub(crate) async fn try_friendship_actions(
    pool: &PgPool,
    viewer: &Address,
    addresses: &[String],
) -> sqlx::Result<HashMap<String, FriendshipAction>> {
    let mut out = HashMap::new();
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (f.id) \
           CASE WHEN f.address_requester = $1 THEN f.address_requested \
                ELSE f.address_requester END AS other_user, \
           fa.action, \
           fa.acting_user \
         FROM social.friendships f \
         INNER JOIN social.friendship_actions fa ON f.id = fa.friendship_id \
         WHERE (f.address_requester = $1 AND f.address_requested = ANY($2)) \
            OR (f.address_requested = $1 AND f.address_requester = ANY($2)) \
         ORDER BY f.id, fa.timestamp DESC, fa.id DESC",
    )
    .bind(viewer)
    .bind(addresses)
    .fetch_all(pool)
    .await?;
    for (other_user, action, acting_user) in rows {
        out.insert(
            other_user.to_lowercase(),
            FriendshipAction {
                action,
                acting_user,
            },
        );
    }
    Ok(out)
}

/// Up to three of the viewer's friends per community (by address order),
/// for the `friends` decoration of the listing.
pub async fn community_friends(
    pool: &PgPool,
    viewer: &Address,
    community_ids: &[CommunityId],
) -> sqlx::Result<HashMap<CommunityId, Vec<String>>> {
    let mut out: HashMap<CommunityId, Vec<String>> = HashMap::new();
    let friend_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT CASE WHEN f.address_requester = $1 \
                                THEN f.address_requested \
                                ELSE f.address_requester END AS address \
         FROM social.friendships f \
         WHERE f.is_active = TRUE \
           AND (f.address_requester = $1 OR f.address_requested = $1)",
    )
    .bind(viewer)
    .fetch_all(pool)
    .await?;
    let friends: Vec<String> = friend_rows.into_iter().map(|(a,)| a).collect();
    if friends.is_empty() {
        return Ok(out);
    }
    let rows: Vec<(CommunityId, String)> = sqlx::query_as(
        "SELECT community_id, member_address FROM ( \
           SELECT cm.community_id, cm.member_address, \
                  ROW_NUMBER() OVER (PARTITION BY cm.community_id ORDER BY cm.member_address) AS rn \
           FROM social.community_members cm \
           WHERE cm.community_id = ANY($1) AND cm.member_address = ANY($2) \
         ) ranked \
         WHERE rn <= 3",
    )
    .bind(community_ids)
    .bind(&friends)
    .fetch_all(pool)
    .await?;
    for (community_id, address) in rows {
        out.entry(community_id).or_default().push(address);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::friendship_request_status;
    use super::friendship_status::*;

    const ME: &str = "0xaaa";
    const OTHER: &str = "0xbbb";

    #[test]
    fn request_direction_follows_the_acting_user() {
        assert_eq!(friendship_request_status("request", ME, ME), REQUEST_SENT);
        assert_eq!(
            friendship_request_status("request", OTHER, ME),
            REQUEST_RECEIVED
        );
        assert_eq!(
            friendship_request_status("request", "0xAAA", ME),
            REQUEST_SENT
        );
    }

    #[test]
    fn blocked_vs_blocked_by() {
        assert_eq!(friendship_request_status("block", ME, ME), BLOCKED);
        assert_eq!(friendship_request_status("block", OTHER, ME), BLOCKED_BY);
        assert_eq!(friendship_request_status("block", "0xAAA", ME), BLOCKED);
    }

    #[test]
    fn terminal_actions_ignore_acting_user() {
        assert_eq!(friendship_request_status("accept", OTHER, ME), ACCEPTED);
        assert_eq!(friendship_request_status("cancel", OTHER, ME), CANCELED);
        assert_eq!(friendship_request_status("delete", OTHER, ME), DELETED);
        assert_eq!(friendship_request_status("reject", OTHER, ME), REJECTED);
    }

    #[test]
    fn unknown_action_is_unrecognized_and_none_is_seven() {
        assert_eq!(
            friendship_request_status("frobnicate", ME, ME),
            UNRECOGNIZED
        );
        assert_eq!(NONE, 7);
    }
}

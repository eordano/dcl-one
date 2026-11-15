//! The user notices worlds produces, with the titles, descriptions and links
//! of the old service's events as the notifications service stored them:
//! `worlds_permission_granted` / `worlds_permission_revoked` (one per
//! address a grant edit actually added or removed) and the storage-quota
//! trio `worlds_missing_resources`, `worlds_access_restricted`,
//! `worlds_access_restored`. Rows are written through social's
//! [`SocialDirectory`] in the caller's transaction.

use std::sync::Arc;

use chrono::NaiveDateTime;
use serde_json::json;
use uuid::Uuid;

use crate::comms::access::QUOTA_BLOCK_GRACE;
use crate::comms::interfaces::{SocialDirectory, UserNotification};
use crate::comms::world_identity::WorldName;
use crate::policy::Permission;
use crate::shared::identity::Address;

/// WCS `BUILDER_URL`'s default.
pub const DEFAULT_BUILDER_URL: &str = "https://decentraland.org/builder";

pub const PERMISSION_GRANTED: &str = "worlds_permission_granted";
pub const PERMISSION_REVOKED: &str = "worlds_permission_revoked";
pub const MISSING_RESOURCES: &str = "worlds_missing_resources";
pub const ACCESS_RESTRICTED: &str = "worlds_access_restricted";
pub const ACCESS_RESTORED: &str = "worlds_access_restored";

/// Namespace of the quota notices' ids (v5 over kind, event key and wallet).
const QUOTA_NS: Uuid = Uuid::from_bytes([
    0x5b, 0x0e, 0x7c, 0x31, 0x9a, 0x4d, 0x4f, 0x8e, 0xa1, 0x52, 0x3c, 0x77, 0x08, 0xd4, 0x6e, 0x19,
]);

/// Namespace of the permission notices' ids (v5 over kind, world, permission, recipient and change time).
const PERMISSION_NS: Uuid = Uuid::from_bytes([
    0x2f, 0x81, 0x4a, 0xc6, 0x13, 0x5e, 0x4b, 0x07, 0x9d, 0x3a, 0x60, 0xe2, 0x71, 0xbc, 0x58, 0x0d,
]);

pub struct Notices {
    social: Option<Arc<dyn SocialDirectory>>,
    builder_url: String,
}

impl Default for Notices {
    fn default() -> Self {
        Self::new(None, DEFAULT_BUILDER_URL)
    }
}

impl Notices {
    /// `social: None` (tests, tools) drops every notice.
    pub fn new(social: Option<Arc<dyn SocialDirectory>>, builder_url: &str) -> Self {
        Self {
            social,
            builder_url: builder_url.trim_end_matches('/').to_string(),
        }
    }

    fn worlds_url(&self) -> String {
        format!("{}/worlds?tab=dcl", self.builder_url)
    }

    /// Writes `notices` in `transaction`; a no-op without a directory.
    pub async fn send(
        &self,
        transaction: &mut sqlx::PgConnection,
        notices: &[UserNotification],
    ) -> Result<(), sqlx::Error> {
        match (&self.social, notices.is_empty()) {
            (Some(social), false) => social.notify_user_notifications(transaction, notices).await,
            _ => Ok(()),
        }
    }

    /// One notice per address whose `permission` grant on `world` was added
    /// (`granted`) or removed.
    pub fn permission_changed(
        &self,
        granted: bool,
        world: &WorldName,
        permission: Permission,
        addresses: &[String],
        now: NaiveDateTime,
    ) -> Vec<UserNotification> {
        let (kind, title) = if granted {
            (PERMISSION_GRANTED, "World Permission Granted")
        } else {
            (PERMISSION_REVOKED, "World Permission Revoked")
        };
        let (world, permission) = (world.key(), permission.as_str());
        let target = if super::reprocess::matches_world_name_pattern(world) {
            format!("world {world}")
        } else {
            "a world".to_string()
        };
        let description = if granted {
            format!("You have been granted {permission} permission for {target}.")
        } else {
            format!("Your {permission} permission for {target} has been revoked.")
        };
        // Keyed on the change itself, so a later change is always a new notice.
        let version = now.and_utc().timestamp_micros();
        addresses
            .iter()
            .filter_map(|a| Address::parse(a).ok())
            .map(|recipient| UserNotification {
                id: Uuid::new_v5(
                    &PERMISSION_NS,
                    format!(
                        "{kind}\n{world}\n{permission}\n{}\n{version}",
                        recipient.as_str()
                    )
                    .as_bytes(),
                ),
                metadata: json!({
                    "title": title,
                    "description": description,
                    "world": world,
                    "permissions": [permission],
                    "url": self.worlds_url(),
                    "address": recipient.as_str(),
                }),
                recipient,
                kind,
                occurred_at: now,
            })
            .collect()
    }

    /// The block of `wallet` created at `created_at`, refreshed at
    /// `updated_at`: a warning inside the grace, the restriction after it.
    pub fn quota_block(
        &self,
        wallet: &Address,
        created_at: NaiveDateTime,
        updated_at: NaiveDateTime,
    ) -> UserNotification {
        let ends = created_at + QUOTA_BLOCK_GRACE;
        let when = ends.and_utc().timestamp_millis();
        // Restricted only once access is refused (`world_blocked`'s `>`).
        if updated_at - created_at <= QUOTA_BLOCK_GRACE {
            quota_notice(
                MISSING_RESOURCES,
                wallet,
                created_at,
                created_at,
                json!({
                    "title": "Missing Resources",
                    "description": "World access at risk in 48hs. Rectify now to prevent disruption.",
                    "url": self.worlds_url(),
                    "when": when,
                    "address": wallet.as_str(),
                }),
            )
        } else {
            quota_notice(
                ACCESS_RESTRICTED,
                wallet,
                created_at,
                ends,
                json!({
                    "title": "Worlds restricted",
                    "description": "Access to your Worlds has been restricted due to insufficient resources.",
                    "when": when,
                    "address": wallet.as_str(),
                }),
            )
        }
    }

    /// The block of `wallet` created at `created_at` was lifted at `now`.
    pub fn quota_restored(
        &self,
        wallet: &Address,
        created_at: NaiveDateTime,
        now: NaiveDateTime,
    ) -> UserNotification {
        quota_notice(
            ACCESS_RESTORED,
            wallet,
            created_at,
            now,
            json!({
                "title": "Worlds available",
                "description": "Access to your Worlds has been restored.",
                "url": self.worlds_url(),
                "attendee": wallet.as_str(),
            }),
        )
    }
}

/// The old `detected-<...>` event key per kind and wallet, keyed on the
/// block's full creation time so a later block is a new notice.
fn quota_notice(
    kind: &'static str,
    wallet: &Address,
    created_at: NaiveDateTime,
    occurred_at: NaiveDateTime,
    metadata: serde_json::Value,
) -> UserNotification {
    let key = format!("detected-{}", created_at.and_utc().timestamp_micros());
    let identity = format!("{kind}\n{key}\n{}", wallet.as_str());
    UserNotification {
        id: Uuid::new_v5(&QUOTA_NS, identity.as_bytes()),
        recipient: wallet.clone(),
        kind,
        metadata,
        occurred_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WALLET: &str = "0x1111111111111111111111111111111111111111";

    fn at(secs: i64) -> NaiveDateTime {
        chrono::DateTime::from_timestamp(secs, 0)
            .unwrap()
            .naive_utc()
    }

    #[test]
    fn permission_notices_carry_the_old_titles_and_link() {
        let notices = Notices::new(None, "https://builder.example/");
        let world = WorldName::parse("Foo.dcl.eth").unwrap();
        let out = notices.permission_changed(
            true,
            &world,
            Permission::Deployment,
            &[WALLET.to_string()],
            at(0),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, PERMISSION_GRANTED);
        assert_eq!(
            out[0].metadata,
            json!({
                "title": "World Permission Granted",
                "description": "You have been granted deployment permission for world foo.dcl.eth.",
                "world": "foo.dcl.eth",
                "permissions": ["deployment"],
                "url": "https://builder.example/worlds?tab=dcl",
                "address": WALLET,
            })
        );
        let out = notices.permission_changed(
            false,
            &world,
            Permission::Streaming,
            &[WALLET.to_string()],
            at(0),
        );
        assert_eq!(out[0].kind, PERMISSION_REVOKED);
        assert_eq!(out[0].metadata["title"], "World Permission Revoked");
        assert_eq!(
            out[0].metadata["description"],
            "Your streaming permission for world foo.dcl.eth has been revoked."
        );
    }

    #[test]
    fn a_name_outside_the_world_name_pattern_stays_out_of_the_text() {
        let notices = Notices::default();
        for raw in ["<b>x.dcl.eth", "foo.dcl.eth.com", "x_y.eth", "foo"] {
            let world = WorldName::parse(raw).unwrap();
            let changed = |granted| {
                notices.permission_changed(
                    granted,
                    &world,
                    Permission::Access,
                    &[WALLET.to_string()],
                    at(0),
                )[0]
                .metadata["description"]
                    .clone()
            };
            assert_eq!(
                changed(true),
                "You have been granted access permission for a world."
            );
            assert_eq!(
                changed(false),
                "Your access permission for a world has been revoked."
            );
        }
    }

    #[test]
    fn each_permission_change_is_its_own_notice() {
        let notices = Notices::default();
        let world = WorldName::parse("foo.dcl.eth").unwrap();
        let id = |granted: bool, permission, secs| {
            notices.permission_changed(granted, &world, permission, &[WALLET.to_string()], at(secs))
                [0]
            .id
        };
        let base = 3600 * 500_000;
        let first = id(true, Permission::Deployment, base);
        assert_eq!(first, id(true, Permission::Deployment, base));
        // Grant, revoke, grant within the hour: the last grant is not dropped.
        assert_ne!(first, id(true, Permission::Deployment, base + 2));
        assert_ne!(first, id(false, Permission::Deployment, base));
        assert_ne!(first, id(true, Permission::Streaming, base));
        let other = notices.permission_changed(
            true,
            &world,
            Permission::Deployment,
            &["0x2222222222222222222222222222222222222222".to_string()],
            at(base),
        );
        assert_ne!(first, other[0].id);
    }

    #[test]
    fn a_second_block_on_the_same_day_is_a_new_notice() {
        let notices = Notices::default();
        let wallet = Address::parse(WALLET).unwrap();
        let first = at(1_800_000_000);
        let second = first + chrono::TimeDelta::hours(1);
        assert_ne!(
            notices.quota_block(&wallet, first, first).id,
            notices.quota_block(&wallet, second, second).id
        );
        assert_ne!(
            notices.quota_restored(&wallet, first, second).id,
            notices.quota_restored(&wallet, second, second).id
        );
    }

    #[test]
    fn a_block_warns_inside_the_grace_and_restricts_after_it() {
        let notices = Notices::new(None, "https://b.example");
        let wallet = Address::parse(WALLET).unwrap();
        let created = at(1_800_000_000);
        let ends = created + QUOTA_BLOCK_GRACE;
        let warn = notices.quota_block(&wallet, created, created);
        assert_eq!(warn.kind, MISSING_RESOURCES);
        assert_eq!(warn.occurred_at, created);
        assert_eq!(
            warn.metadata,
            json!({
                "title": "Missing Resources",
                "description": "World access at risk in 48hs. Rectify now to prevent disruption.",
                "url": "https://b.example/worlds?tab=dcl",
                "when": ends.and_utc().timestamp_millis(),
                "address": WALLET,
            })
        );
        // The same block re-detected keeps its id; the restriction is another notice.
        let again = notices.quota_block(&wallet, created, created + chrono::TimeDelta::hours(12));
        assert_eq!(again.id, warn.id);
        assert_eq!(notices.quota_block(&wallet, created, ends).id, warn.id);
        let after = ends + chrono::TimeDelta::seconds(1);
        let restricted = notices.quota_block(&wallet, created, after);
        assert_eq!(restricted.kind, ACCESS_RESTRICTED);
        assert_eq!(restricted.occurred_at, ends);
        assert_ne!(restricted.id, warn.id);
        assert_eq!(restricted.metadata.get("url"), None);
        assert_eq!(restricted.metadata["address"], WALLET);
        let restored = notices.quota_restored(&wallet, created, ends);
        assert_eq!(restored.kind, ACCESS_RESTORED);
        assert_eq!(
            restored.metadata,
            json!({
                "title": "Worlds available",
                "description": "Access to your Worlds has been restored.",
                "url": "https://b.example/worlds?tab=dcl",
                "attendee": WALLET,
            })
        );
    }
}

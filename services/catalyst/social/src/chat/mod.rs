//! Chat: the "Decentraland standalone chat" (<domain>/social, chat.<domain>,
//! dcl.social) over schema `chat` (`schemas/45_chat.sql`). Community
//! channels, messages, threads,
//! reactions, reports, pins, the activity feed and the open lounges are this
//! node's durable writes behind [`super::writes`]; membership, roles, bans,
//! posts, places, voice lounges and profiles are `super::communities`,
//! `super::comms` and the profile readers, called in-process.
//!
//! This file holds the declaration and the types the schema binds to; the
//! route port (the old `catalyrst/crates/dcl-social-api`) lands in
//! submodules beside it.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A history or thread page: the reads select `PAGE + 1` rows to learn
/// whether more exist, then return `PAGE`.
pub const PAGE: usize = 100;
/// Replies carried inline with each message of a history page.
pub const INLINE_REPLIES: usize = 100;
/// Channels a community may have, `general` excluded.
pub const MAX_CHANNELS: i64 = 50;
/// The longest message or reply text (`chat.messages`/`chat.replies` CHECK).
pub const MAX_TEXT_CHARS: usize = 4000;
/// The longest activity summary (`chat.community_activity` CHECK).
pub const MAX_SUMMARY_CHARS: usize = 140;
/// Wallets a private channel's `config.members` may list.
pub const MAX_CHANNEL_MEMBERS: usize = 100;
/// The longest slow mode a channel may set.
pub const MAX_SLOW_MODE: Duration = Duration::from_secs(3600);
/// A lounge nobody has seen for this long counts as a new session when it
/// is seen again (`chat.community_live`).
pub const LIVE_GAP: Duration = Duration::from_secs(600);
/// A prepared action lives this long (`chat.actions.expires_at`).
pub const ACTION_TTL: Duration = Duration::from_secs(90);
/// A read ticket lives at most this long past its signature's timestamp.
pub const READ_WINDOW: Duration = Duration::from_secs(50);
/// Live `chat.actions` rows past which `prepare` answers 429.
pub const MAX_LIVE_ACTIONS: i64 = 10_000;
/// The channel every community has.
pub const GENERAL: &str = "general";

/// A channel name as `chat.channels.name` accepts it: `general` or 1 to 32
/// characters of `[a-z0-9-]` (the DDL CHECK is the same expression).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ChannelName(String);

impl ChannelName {
    pub fn parse(name: &str) -> Option<Self> {
        let ok = !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
        ok.then(|| Self(name.to_owned()))
    }

    pub fn general() -> Self {
        Self(GENERAL.to_owned())
    }

    pub fn is_general(&self) -> bool {
        self.0 == GENERAL
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChannelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ChannelName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom("a channel name is 1-32 characters of a-z, 0-9 and -")
        })
    }
}

/// `chat.channels.config`, read and written as one unit. `private` is the
/// typed column beside it, not a key of this document; absent keys mean
/// their defaults, so a fresh channel's `'{}'` deserializes here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelConfig {
    /// Lowercase wallets admitted to a private channel besides moderators.
    pub members: Vec<String>,
    pub read_only: bool,
    /// Seconds between two messages of one wallet; 0 is off.
    pub slow_mode: u64,
}

impl ChannelConfig {
    /// The old service's `validate` plus the lowercase form the schema's
    /// wallet CHECKs expect; `Err` carries the message the wire answered.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.members.len() > MAX_CHANNEL_MEMBERS
            || self.slow_mode > MAX_SLOW_MODE.as_secs()
            || !self.members.iter().all(|w| crate::auth::is_eth_address(w))
        {
            return Err("Use valid wallet addresses and a slow mode of up to one hour");
        }
        Ok(())
    }

    /// The same document with every member lowercased, as the schema stores
    /// wallets and the admission check compares them.
    pub fn normalized(mut self) -> Self {
        for member in &mut self.members {
            member.make_ascii_lowercase();
        }
        self
    }
}

/// The client's action on a message (`conversation::Action` of the old crate).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAction {
    Heart,
    Celebrate,
    Pin,
    Unpin,
    Report,
    Dismiss,
    Remove,
}

impl MessageAction {
    /// The `chat.reactions.emoji` value of a reaction toggle, `None` for the
    /// moderation and report actions.
    pub fn emoji(self) -> Option<Reaction> {
        match self {
            Self::Heart => Some(Reaction::Heart),
            Self::Celebrate => Some(Reaction::Celebrate),
            _ => None,
        }
    }

    /// Only owners and moderators may do it.
    pub fn moderator_only(self) -> bool {
        matches!(self, Self::Pin | Self::Unpin | Self::Dismiss | Self::Remove)
    }
}

/// `chat.reactions.emoji` (its CHECK lists exactly these).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reaction {
    Heart,
    Celebrate,
}

impl Reaction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Heart => "heart",
            Self::Celebrate => "celebrate",
        }
    }
}

/// `chat.community_activity.kind` (its CHECK lists exactly these).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Announcement,
    Hangout,
    Lounge,
}

impl ActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Announcement => "announcement",
            Self::Hangout => "hangout",
            Self::Lounge => "lounge",
        }
    }
}

/// `chat.actions.state` (its CHECK lists exactly these): `pending` until
/// `complete` claims it, `submitted` after, `read` for a history ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Pending,
    Submitted,
    Read,
}

impl ActionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Submitted => "submitted",
            Self::Read => "read",
        }
    }
}

/// The wire's millisecond epoch of a `timestamp without time zone` UTC column.
pub fn epoch_millis(at: chrono::NaiveDateTime) -> i64 {
    at.and_utc().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_names_follow_the_schema_check() {
        for ok in [
            "general",
            "a",
            "dev-talk",
            "0123456789abcdefghijklmnopqrstuv",
        ] {
            assert!(ChannelName::parse(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            "General",
            "dev talk",
            "dev_talk",
            "0123456789abcdefghijklmnopqrstuvw",
            "ñ",
        ] {
            assert!(ChannelName::parse(bad).is_none(), "{bad:?}");
        }
        assert!(ChannelName::general().is_general());
    }

    #[test]
    fn a_fresh_channel_config_is_the_empty_document() {
        let config: ChannelConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, ChannelConfig::default());
        assert!(config.validate().is_ok());
        assert!(
            serde_json::from_str::<ChannelConfig>(r#"{"private":true}"#).is_err(),
            "private is the typed column"
        );
    }

    #[test]
    fn channel_config_limits_match_the_old_service() {
        let too_slow = ChannelConfig {
            slow_mode: 3601,
            ..Default::default()
        };
        assert!(too_slow.validate().is_err());
        let bad_wallet = ChannelConfig {
            members: vec!["0xnothex".into()],
            ..Default::default()
        };
        assert!(bad_wallet.validate().is_err());
        let checksummed = ChannelConfig {
            members: vec![format!("0x{}", "A".repeat(40))],
            ..Default::default()
        };
        assert!(checksummed.validate().is_ok());
        assert_eq!(
            checksummed.normalized().members[0],
            format!("0x{}", "a".repeat(40))
        );
        let crowd = ChannelConfig {
            members: vec![format!("0x{}", "1".repeat(40)); 101],
            ..Default::default()
        };
        assert!(crowd.validate().is_err());
    }

    #[test]
    fn actions_split_into_reactions_and_moderation() {
        assert_eq!(MessageAction::Heart.emoji(), Some(Reaction::Heart));
        assert_eq!(MessageAction::Celebrate.emoji(), Some(Reaction::Celebrate));
        assert_eq!(MessageAction::Report.emoji(), None);
        assert!(!MessageAction::Report.moderator_only());
        assert!(MessageAction::Remove.moderator_only());
        assert_eq!(
            serde_json::to_string(&MessageAction::Unpin).unwrap(),
            "\"unpin\""
        );
    }
}

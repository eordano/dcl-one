//! Archive reads retain one complete Pulse statistics view across content I/O.
use std::sync::Arc;

use async_trait::async_trait;

use super::model::{HotScene, Island, Peer};
use crate::comms::gatekeeper;
use crate::comms::gatekeeper::admission::handlers::{ParticipantsQuery, participants};
use crate::comms::{protocol::bus::statistics::Peer as StatisticalPeer, stats};

/// The worlds presence map as `/live-data` serves it.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveData {
    pub total_users: i64,
    pub per_world: Vec<(String, i64)>,
}

#[async_trait]
pub trait Sources: Send + Sync {
    async fn statistics(&self) -> anyhow::Result<Statistics>;
    /// The wallets in the room of the Genesis scene at `pointer` in `realm`,
    /// as reported (the collector lowercases and deduplicates them); `None`
    /// when no scene is deployed there (the old 404, which skipped the
    /// occupancy row).
    async fn scene_participants(
        &self,
        realm: &str,
        pointer: &str,
    ) -> anyhow::Result<Option<Vec<String>>>;
    /// The wallets in a world's room; `None` when the world is unknown.
    async fn world_participants(&self, world: &str) -> anyhow::Result<Option<Vec<String>>>;
    /// The worlds presence map; `None` when this node serves none.
    async fn worlds_live_data(&self) -> anyhow::Result<Option<LiveData>>;
}

/// [`Sources`] over the in-process archipelago, comms and worlds states.
pub struct ModuleSources {
    pub statistics: Arc<stats::Reader>,
    pub content: Arc<dyn stats::content::Source>,
    pub content_url: String,
    pub comms: gatekeeper::SharedState,
    pub worlds: crate::worlds::core::SharedState,
}

impl ModuleSources {
    pub fn new(
        statistics: Arc<stats::Reader>,
        content: Arc<dyn stats::content::Source>,
        content_url: String,
        comms: gatekeeper::SharedState,
        worlds: crate::worlds::core::SharedState,
    ) -> Arc<Self> {
        Arc::new(Self {
            statistics,
            content,
            content_url,
            comms,
            worlds,
        })
    }

    async fn participants(&self, query: ParticipantsQuery) -> anyhow::Result<Option<Vec<String>>> {
        match participants(&self.comms, query).await {
            Ok(addresses) => Ok(Some(addresses)),
            Err(e) if e.code == 404 => Ok(None),
            Err(e) => anyhow::bail!("scene-participants answered {}: {}", e.code, e.message),
        }
    }
}

#[async_trait]
impl Sources for ModuleSources {
    async fn statistics(&self) -> anyhow::Result<Statistics> {
        statistics(&self.statistics, self.content.as_ref(), &self.content_url).await
    }

    async fn scene_participants(
        &self,
        realm: &str,
        pointer: &str,
    ) -> anyhow::Result<Option<Vec<String>>> {
        self.participants(ParticipantsQuery {
            pointer: Some(pointer.to_string()),
            realm_name: Some(realm.to_string()),
            room: None,
        })
        .await
    }

    async fn world_participants(&self, world: &str) -> anyhow::Result<Option<Vec<String>>> {
        self.participants(ParticipantsQuery {
            pointer: None,
            realm_name: Some(world.to_string()),
            room: None,
        })
        .await
    }

    async fn worlds_live_data(&self) -> anyhow::Result<Option<LiveData>> {
        let per_world = self.worlds.presence.world_counts();
        let total_users = per_world.iter().map(|(_, n)| n).sum();
        Ok(Some(LiveData {
            total_users,
            per_world,
        }))
    }
}

pub struct Statistics {
    pub peers: Vec<Peer>,
    pub islands: Vec<Island>,
    pub hot_scenes: Vec<HotScene>,
}
/// Acquire once before content I/O; every projection uses this same retained Arc.
pub async fn statistics(
    reader: &stats::Reader,
    content: &dyn stats::content::Source,
    url: &str,
) -> anyhow::Result<Statistics> {
    let snapshot = reader.snapshot();
    let tiles = stats::render::tiles(&snapshot);
    let documents = stats::content::tile_documents(content, &tiles)
        .await
        .map_err(|_| anyhow::anyhow!("archive active content query failed"))?;
    let hot_scenes = stats::content::project_hot_scenes(&documents, &tiles, url)
        .map_err(|_| anyhow::anyhow!("archive hot scene projection failed"))?
        .into_iter()
        .map(|scene| {
            Ok(HotScene {
                scene_id: scene
                    .id
                    .and_then(crate::content::OrderedJson::as_str)
                    .ok_or_else(|| anyhow::anyhow!("archive scene id is not a string"))?
                    .to_owned(),
                name: scene
                    .name
                    .and_then(crate::content::OrderedJson::as_str)
                    .map(str::to_owned),
                base_x: scene.base_coords.first().copied().and_then(integer),
                base_y: scene.base_coords.get(1).copied().and_then(integer),
                users_count: Some(i32::try_from(scene.users_total_count)?),
                parcel_count: i32::try_from(scene.parcels.len())?,
                creator: scene
                    .creator
                    .and_then(crate::content::OrderedJson::as_str)
                    .map(str::to_owned),
                description: scene
                    .description
                    .and_then(crate::content::OrderedJson::as_str)
                    .map(str::to_owned),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Statistics {
        peers: snapshot.peers.iter().map(peer_of).collect(),
        islands: stats::render::public_islands(&snapshot)
            .into_iter()
            .map(|island| {
                Ok(Island {
                    island_id: island.id,
                    peer_count: i32::try_from(island.peers.len())?,
                    max_peers: Some(i32::try_from(island.max_peers)?),
                    center_x: island.center[0].is_finite().then_some(island.center[0]),
                    center_y: island.center[1].is_finite().then_some(island.center[1]),
                    center_z: island.center[2].is_finite().then_some(island.center[2]),
                    radius: island.radius.is_finite().then_some(island.radius),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        hot_scenes,
    })
}
fn integer(value: f64) -> Option<i32> {
    (value.is_finite() && value >= i32::MIN as f64 && value <= i32::MAX as f64)
        .then_some(value as i32)
}
pub fn peer_of(peer: &StatisticalPeer) -> Peer {
    let [x, y] = stats::render::parcel(peer);
    Peer {
        address: peer.address.clone(),
        parcel_x: integer(x),
        parcel_y: integer(y),
        position_x: peer.position[0].is_finite().then_some(peer.position[0]),
        position_y: peer.position[1].is_finite().then_some(peer.position[1]),
        position_z: peer.position[2].is_finite().then_some(peer.position[2]),
        last_ping: Some(peer.time),
    }
}

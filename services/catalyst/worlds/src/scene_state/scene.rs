//! One hosted scene: its runtime, its connected clients and the entity
//! range each was given. Removing a client closes its socket (the
//! `CancellationToken` the socket loop watches), so a kick, a revocation and
//! an admin reset all end the connection rather than muting it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::comms::world_identity::{SceneId, SceneNamespace};
use crate::policy::AccessGrant;
use crate::rooms::ActiveDeployment;
use crate::scene_state::SceneKey;
use crate::scene_state::runtime::SceneRuntime;

/// Connection ids are never reused, unlike entity-range indices.
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Client {
    pub index: u32,
    /// This connection, as opposed to whichever holds `index` later.
    pub id: u64,
    /// Lowercase wallet address (the verified SIGNER).
    pub address: String,

    pub tx: mpsc::Sender<Vec<u8>>,

    /// The entity range this client was **actually given** at open time.
    ///
    /// Persisted rather than recomputed, because the scene can rewrite the
    /// transport config from `onUpdate` (`registerScene`). Recomputing at close
    /// time made a departing client's reclaim either leak its entities or wipe
    /// a live neighbour's; recomputing at message time made two live clients
    /// answer to the same window.
    pub start: u32,
    pub size: u32,

    /// Cancelled when the client is removed: the socket loop ends and closes.
    pub cancel: CancellationToken,

    /// How the permission core admitted this client (`None` for the local
    /// scene and Genesis scenes, which admit anyone not banned).
    pub grant: Option<AccessGrant>,
}

/// A connected client as the sweep sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub index: u32,
    pub id: u64,
    pub address: String,
    pub grant: Option<AccessGrant>,
}

/// What a hosted scene is, for admission and the redeploy sweep.
#[derive(Debug, Clone)]
pub struct SceneIdentity {
    pub key: SceneKey,
    /// The active deployment the scene was loaded from; `None` for the local
    /// development scene.
    pub deployment: Option<ActiveDeployment>,
}

impl SceneIdentity {
    pub fn local() -> Self {
        Self {
            key: SceneKey::Local,
            deployment: None,
        }
    }

    pub fn scene_id(&self) -> Option<&SceneId> {
        self.deployment.as_ref().map(|d| &d.scene_id)
    }

    pub fn namespace(&self) -> Option<&SceneNamespace> {
        self.deployment.as_ref().and_then(|d| d.namespace.as_ref())
    }
}

pub struct Scene {
    pub name: String,
    pub identity: SceneIdentity,
    pub runtime: Arc<dyn SceneRuntime>,
    clients: DashMap<u32, Arc<Client>>,

    /// When the last client left (or the scene loaded with none). Held while
    /// `clients` changes, so the two never disagree.
    empty_since: Mutex<Option<std::time::Instant>>,

    renewal: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Scene {
    pub fn new(name: impl Into<String>, runtime: Arc<dyn SceneRuntime>) -> Self {
        Self::new_with_renewal(name, runtime, None, SceneIdentity::local())
    }

    pub fn new_with_renewal(
        name: impl Into<String>,
        runtime: Arc<dyn SceneRuntime>,
        renewal: Option<tokio::task::JoinHandle<()>>,
        identity: SceneIdentity,
    ) -> Self {
        Self {
            name: name.into(),
            identity,
            runtime,
            clients: DashMap::new(),
            empty_since: Mutex::new(Some(std::time::Instant::now())),
            renewal: Mutex::new(renewal),
        }
    }

    pub fn scene_hash(&self) -> String {
        self.runtime.scene_hash().to_string()
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// How long the scene has had no clients, `None` while any is connected.
    pub fn idle_for(&self) -> Option<std::time::Duration> {
        let since = self.empty_since.lock();
        if !self.clients.is_empty() {
            return None;
        }
        since.map(|since| since.elapsed())
    }

    /// Every connected client, in index order.
    pub fn clients(&self) -> Vec<ClientInfo> {
        let mut out: Vec<ClientInfo> = self
            .clients
            .iter()
            .map(|e| ClientInfo {
                index: *e.key(),
                id: e.value().id,
                address: e.value().address.clone(),
                grant: e.value().grant,
            })
            .collect();
        out.sort_by_key(|c| c.index);
        out
    }

    pub fn add_client(
        &self,
        address: String,
        tx: mpsc::Sender<Vec<u8>>,
    ) -> (Arc<Client>, crate::scene_state::runtime::InitState) {
        self.add_client_admitted(address, tx, None)
    }

    /// `grant` is what [`crate::scene_state::ws::admit`] answered.
    pub fn add_client_admitted(
        &self,
        address: String,
        tx: mpsc::Sender<Vec<u8>>,
        grant: Option<AccessGrant>,
    ) -> (Arc<Client>, crate::scene_state::runtime::InitState) {
        let index = self.runtime.allocate_client_index();
        let init = self.runtime.on_client_open(index, tx.clone());
        let client = Arc::new(Client {
            index,
            id: NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed),
            address,
            tx,
            start: init.start,
            size: init.size,
            cancel: CancellationToken::new(),
            grant,
        });
        let mut since = self.empty_since.lock();
        self.clients.insert(index, Arc::clone(&client));
        *since = None;
        drop(since);
        (client, init)
    }

    /// The range `index` was assigned at open time, if it is still connected.
    pub fn client_range(&self, index: u32) -> Option<(u32, u32)> {
        self.clients.get(&index).map(|c| (c.start, c.size))
    }

    pub fn broadcast(&self, frame: &[u8], except: u32) {
        for entry in self.clients.iter() {
            if *entry.key() == except {
                continue;
            }

            let _ = entry.value().tx.try_send(frame.to_vec());
        }
    }

    pub fn remove_client(&self, index: u32) {
        self.remove_matching(index, |_| true);
    }

    /// Removes connection `id` if it still holds `index`: a decision made
    /// across an await never removes a newer connection given the same index.
    pub fn remove_connection(&self, index: u32, id: u64) -> bool {
        self.remove_matching(index, |c| c.id == id)
    }

    fn remove_matching(&self, index: u32, matches: impl FnOnce(&Client) -> bool) -> bool {
        let mut since = self.empty_since.lock();
        let Some((_, client)) = self.clients.remove_if(&index, |_, c| matches(c)) else {
            return false;
        };
        client.cancel.cancel();
        if self.clients.is_empty() {
            since.get_or_insert_with(std::time::Instant::now);
        }
        drop(since);

        // Only the removal that took the client tells the runtime.
        for body in self.runtime.on_client_close(index) {
            let frame = crate::scene_state::runtime::frame_crdt(&body);
            self.broadcast(&frame, index);
        }
        true
    }

    pub fn kick_all(&self) -> usize {
        let held: Vec<(u32, u64)> = self
            .clients
            .iter()
            .map(|e| (*e.key(), e.value().id))
            .collect();
        held.into_iter()
            .filter(|(index, id)| self.remove_connection(*index, *id))
            .count()
    }

    /// Removes every connection `address` holds on this scene.
    pub fn kick_address(&self, address: &str) -> usize {
        let held: Vec<(u32, u64)> = self
            .clients
            .iter()
            .filter(|e| e.value().address.eq_ignore_ascii_case(address))
            .map(|e| (*e.key(), e.value().id))
            .collect();
        held.into_iter()
            .filter(|(index, id)| self.remove_connection(*index, *id))
            .count()
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.runtime.snapshot()
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        if let Some(task) = self.renewal.lock().take() {
            task.abort();
        }
    }
}

pub struct SceneManager {
    scenes: Mutex<std::collections::HashMap<String, Arc<Scene>>>,
    connection_count: AtomicU32,
}

impl Default for SceneManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SceneManager {
    pub fn new() -> Self {
        Self {
            scenes: Mutex::new(std::collections::HashMap::new()),
            connection_count: AtomicU32::new(0),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<Scene>> {
        self.scenes.lock().get(name).cloned()
    }

    pub fn insert(&self, name: impl Into<String>, scene: Arc<Scene>) -> Option<Arc<Scene>> {
        self.scenes.lock().insert(name.into(), scene)
    }

    pub fn remove(&self, name: &str) -> Option<Arc<Scene>> {
        self.scenes.lock().remove(name)
    }

    pub fn len(&self) -> usize {
        self.scenes.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.scenes.lock().is_empty()
    }

    pub fn all(&self) -> Vec<Arc<Scene>> {
        self.scenes.lock().values().cloned().collect()
    }

    /// `<name>:<hash>` per hosted scene, in name order (the `/status` shape).
    pub fn loaded(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .scenes
            .lock()
            .iter()
            .map(|(name, scene)| format!("{name}:{}", scene.scene_hash()))
            .collect();
        out.sort();
        out
    }

    pub fn connections(&self) -> u32 {
        self.connection_count.load(Ordering::Relaxed)
    }

    pub fn on_ws_connected(&self) {
        self.connection_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_ws_closed(&self) {
        self.connection_count.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_state::runtime::{RelayRuntime, ServerTransportConfig};

    #[test]
    fn add_client_assigns_increasing_indices() {
        let scene = Scene::new(
            "localScene",
            Arc::new(RelayRuntime::new("h", ServerTransportConfig::default())),
        );
        let (tx, _rx) = mpsc::channel(16);
        let (a, _ia) = scene.add_client("0x1".into(), tx.clone());
        let (b, _ib) = scene.add_client("0x2".into(), tx);
        assert_eq!(a.index, 0);
        assert_eq!(b.index, 1);
        assert_eq!(scene.client_count(), 2);
    }

    /// D3. The assigned range travels with the client, so nothing downstream
    /// has to re-derive it from a config the scene can move.
    #[test]
    fn client_carries_the_range_it_was_given() {
        let scene = Scene::new(
            "localScene",
            Arc::new(RelayRuntime::new("h", ServerTransportConfig::default())),
        );
        let (tx, _rx) = mpsc::channel(16);
        let (a, ia) = scene.add_client("0x1".into(), tx.clone());
        let (b, ib) = scene.add_client("0x2".into(), tx);

        assert_eq!((a.start, a.size), (ia.start, ia.size));
        assert_eq!((b.start, b.size), (ib.start, ib.size));
        assert_eq!(scene.client_range(a.index), Some((1024, 512)));
        assert_eq!(scene.client_range(b.index), Some((1536, 512)));
        assert_ne!((a.start, a.size), (b.start, b.size));
    }

    /// The refusal condition `ws::handle_socket` checks: a client for which no
    /// representable range is left is handed `EMPTY_RANGE`, whose `size` is 0. It
    /// can author nothing (`decode_client_batch` admits nothing,
    /// `Authority::Client` rejects everything), so the socket layer now closes
    /// rather than serving a connected-but-mute client -- and the slot must come
    /// straight back, or one refusal would burn it forever.
    #[test]
    fn a_client_with_no_representable_range_gets_an_empty_one_and_frees_its_slot() {
        use crate::scene_state::runtime::EMPTY_RANGE;

        let cfg = ServerTransportConfig {
            reserved_local_entities: 512,
            server_network_entities_limit: u32::MAX - 512,
            client_network_entities_limit: 512,
        };
        assert_eq!(cfg.max_client_slots(), 0);

        let scene = Scene::new("localScene", Arc::new(RelayRuntime::new("h", cfg)));
        let (tx, _rx) = mpsc::channel(16);
        let (client, init) = scene.add_client("0x1".into(), tx.clone());
        assert_eq!((init.start, init.size), EMPTY_RANGE);
        assert_eq!(init.size, 0, "this is what ws.rs refuses on");

        scene.remove_client(client.index);
        assert_eq!(scene.client_count(), 0);
        let (again, _) = scene.add_client("0x2".into(), tx);
        assert_eq!(again.index, client.index);
    }

    /// D4. `remove_client` must push the reclaim out to everyone still here.
    #[test]
    fn removing_a_client_broadcasts_the_reclaim() {
        use crate::scene_state::crdt::{CrdtMessage, decode_batch, encode_batch};

        let scene = Scene::new(
            "localScene",
            Arc::new(RelayRuntime::new("h", ServerTransportConfig::default())),
        );
        let (tx_a, _rx_a) = mpsc::channel(16);
        let (tx_b, mut rx_b) = mpsc::channel(16);
        let (a, _) = scene.add_client("0x1".into(), tx_a);
        let (_b, _) = scene.add_client("0x2".into(), tx_b);

        let body = encode_batch(&[CrdtMessage::Put {
            entity: 1100,
            component_id: 7,
            timestamp: 1,
            data: vec![1],
        }]);
        assert_eq!(
            scene.runtime.on_client_crdt(a.index, &body).unwrap().len(),
            1
        );

        scene.remove_client(a.index);

        let frame = rx_b
            .try_recv()
            .expect("the remaining client must be told the departed range is gone");
        assert_eq!(
            frame[0],
            crate::scene_state::protocol::MessageType::Crdt as u8
        );
        assert_eq!(
            decode_batch(&frame[1..]),
            vec![CrdtMessage::DeleteEntity { entity: 1100 }]
        );
    }

    /// A kick ends the connection: the client's token fires, and only the
    /// kicked address's sockets go.
    #[test]
    fn kicking_an_address_cancels_exactly_its_sockets() {
        let scene = Scene::new(
            "localScene",
            Arc::new(RelayRuntime::new("h", ServerTransportConfig::default())),
        );
        let (tx, _rx) = mpsc::channel(16);
        let (a, _) = scene.add_client(
            "0xAAAAaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            tx.clone(),
        );
        let (a2, _) = scene.add_client(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            tx.clone(),
        );
        let (b, _) = scene.add_client("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(), tx);
        assert_eq!(
            scene.kick_address("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            2
        );
        assert!(a.cancel.is_cancelled() && a2.cancel.is_cancelled());
        assert!(!b.cancel.is_cancelled());
        assert_eq!(
            scene.clients(),
            vec![ClientInfo {
                index: b.index,
                id: b.id,
                address: b.address.clone(),
                grant: None
            }]
        );
        assert_eq!(scene.kick_all(), 1);
        assert!(b.cancel.is_cancelled());
    }

    #[test]
    fn a_stale_removal_never_kicks_the_client_now_at_that_index() {
        let scene = Scene::new(
            "localScene",
            Arc::new(RelayRuntime::new("h", ServerTransportConfig::default())),
        );
        let (tx, _rx) = mpsc::channel(16);
        let (old, _) = scene.add_client(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            tx.clone(),
        );
        scene.remove_client(old.index);
        // The same wallet reconnecting: only the connection id tells them apart.
        let (new, _) = scene.add_client("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(), tx);
        assert_eq!(new.index, old.index, "the index is reused");
        assert!(!scene.remove_connection(old.index, old.id));
        assert!(!new.cancel.is_cancelled());
        assert!(scene.remove_connection(new.index, new.id));
        assert!(new.cancel.is_cancelled());
    }
}

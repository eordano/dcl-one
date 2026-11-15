use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::scene_state::crdt::{CrdtEngine, decode_batch};
use crate::scene_state::runtime::{
    ClientSlots, InboundBudget, RuntimeLimits, ServerTransportConfig,
};

use super::fetch;
use super::scene_thread::{SceneScript, run_scene_thread};

static V8_INIT: std::sync::Once = std::sync::Once::new();

/// Initialize the process platform before creating any V8 execution workers.
/// This Once does not repair the PKRU state of preexisting threads.
pub fn ensure_v8_initialized() {
    V8_INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

#[derive(Debug)]
pub enum Command {
    /// A client connected. The range it was told about travels with the
    /// command: the scene thread must judge and reclaim this client against the
    /// window it was *actually given*, not against whatever the config says
    /// later (`registerScene` is callable from `onUpdate`).
    ClientOpen {
        index: u32,
        start: u32,
        size: u32,
    },

    ClientCrdt {
        index: u32,
        body: Vec<u8>,
    },

    ClientClose {
        index: u32,
    },

    Shutdown,
}

pub struct SharedState {
    pub engine: Arc<Mutex<CrdtEngine>>,
    pub snapshot: Arc<Mutex<Vec<u8>>>,
    pub config: Arc<Mutex<ServerTransportConfig>>,

    pub outbound: dashmap::DashMap<u32, mpsc::Sender<Vec<u8>>>,
    /// Bytes handed to each client's socket channel and not yet written.
    pub outbound_meters: dashmap::DashMap<u32, Arc<std::sync::atomic::AtomicUsize>>,
    pub running: AtomicBool,

    /// Bounded, recycled client indices. Shared with the scene thread, which
    /// releases a slot only once it has reclaimed that client's entity range.
    pub slots: Arc<ClientSlots>,

    /// Client bytes queued toward the scene thread and not yet read.
    pub inbound: Arc<InboundBudget>,

    /// Set when a V8 OOM parked the scene thread for good.
    pub parked: AtomicBool,

    /// Client frames this scene dropped because its inbound budget was full.
    pub inbound_drops: AtomicU64,

    /// Clients this scene closed after too many dropped frames.
    pub desynced_closes: AtomicU64,
}

pub(super) struct Watchdog {
    deadline_ms: AtomicU64,

    tick_active: AtomicBool,

    fired: AtomicBool,

    stopped: AtomicBool,

    epoch: std::time::Instant,

    pub(super) isolate: parking_lot::Mutex<Option<v8::IsolateHandle>>,
}

impl Watchdog {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
}

pub struct JsRuntimeHandle {
    pub scene_hash: String,
    pub shared: Arc<SharedState>,
    pub tx: mpsc::UnboundedSender<Command>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,

    watchdog: Arc<Watchdog>,

    shutdown_join: std::time::Duration,
}

impl JsRuntimeHandle {
    /// Take the lowest free client slot. Slots are returned on disconnect, so
    /// a scene that churns connections can no longer walk past the last
    /// representable entity range (and cannot wrap around to index 0).
    pub fn acquire_client_index(&self) -> u32 {
        self.shared.slots.acquire()
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown);
        self.shared.running.store(false, Ordering::SeqCst);

        self.watchdog.stopped.store(true, Ordering::SeqCst);
        if let Some(h) = self.watchdog.isolate.lock().as_ref() {
            h.terminate_execution();
        }

        let mut guard = self.join.lock();
        // A parked thread never ends: detach it without waiting.
        if self.shared.parked.load(Ordering::SeqCst) {
            guard.take();
            return;
        }
        if let Some(j) = guard.as_ref() {
            if j.is_finished() {
                if let Some(j) = guard.take() {
                    let _ = j.join();
                }
                return;
            }
        } else {
            return;
        }
        drop(guard);
        let deadline = std::time::Instant::now() + self.shutdown_join;
        let mut delay = std::time::Duration::from_millis(1);
        loop {
            {
                let guard = self.join.lock();
                match guard.as_ref() {
                    Some(j) if j.is_finished() => {
                        drop(guard);
                        if let Some(j) = self.join.lock().take() {
                            let _ = j.join();
                        }
                        return;
                    }
                    Some(_) => {}
                    None => return,
                }
            }
            if std::time::Instant::now() >= deadline {
                let _ = self.join.lock().take();
                tracing::warn!(
                    scene = %self.scene_hash,
                    "scene JS thread did not stop within shutdown budget; detaching"
                );
                return;
            }
            std::thread::sleep(
                delay.min(deadline.saturating_duration_since(std::time::Instant::now())),
            );
            delay = delay
                .saturating_mul(2)
                .min(std::time::Duration::from_millis(50));
        }
    }
}

impl Drop for JsRuntimeHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn spawn(
    scene_hash: String,
    source: String,
    realm_name: String,
    limits: RuntimeLimits,
    static_crdt: Vec<u8>,
    storage: Option<fetch::StorageCtx>,
) -> JsRuntimeHandle {
    let engine = Arc::new(Mutex::new(CrdtEngine::with_limits(
        limits.crdt_max_components,
        limits.crdt_max_value_bytes,
        limits.crdt_max_bytes,
    )));
    let snapshot = Arc::new(Mutex::new(Vec::new()));

    if !static_crdt.is_empty() {
        let msgs = decode_batch(&static_crdt);
        if !msgs.is_empty() {
            let mut eng = engine.lock();
            eng.apply_batch(&msgs);
            *snapshot.lock() = eng.snapshot();
            tracing::info!(scene = %scene_hash, ops = msgs.len(), "seeded static main.crdt");
        }
    }
    let config = Arc::new(Mutex::new(ServerTransportConfig::default()));
    let slots = Arc::new(ClientSlots::default());
    let shared = Arc::new(SharedState {
        engine: Arc::clone(&engine),
        snapshot: Arc::clone(&snapshot),
        config: Arc::clone(&config),
        outbound: dashmap::DashMap::new(),
        outbound_meters: dashmap::DashMap::new(),
        running: AtomicBool::new(true),
        slots: Arc::clone(&slots),
        inbound: Arc::new(InboundBudget::new(&limits)),
        parked: AtomicBool::new(false),
        inbound_drops: AtomicU64::new(0),
        desynced_closes: AtomicU64::new(0),
    });

    let watchdog = Arc::new(Watchdog {
        deadline_ms: AtomicU64::new(0),
        tick_active: AtomicBool::new(false),
        fired: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        epoch: std::time::Instant::now(),
        isolate: parking_lot::Mutex::new(None),
    });

    let (tx, rx) = mpsc::unbounded_channel::<Command>();

    let fetch_wiring = storage.map(|ctx| {
        let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
        let (results_tx, results_rx) = std::sync::mpsc::channel();
        drop(fetch::spawn_fetch_worker(
            ctx.clone(),
            limits,
            jobs_rx,
            results_tx,
        ));
        fetch::FetchWiring {
            ctx,
            tx: jobs_tx,
            results: results_rx,
        }
    });

    let thread_shared = Arc::clone(&shared);
    let thread_watchdog = Arc::clone(&watchdog);
    let thread_hash = scene_hash.clone();
    let join = std::thread::Builder::new()
        .name(format!("scene-js-{scene_hash}"))
        .spawn(move || {
            run_scene_thread(
                SceneScript {
                    scene_hash: thread_hash,
                    source,
                    realm_name,
                },
                thread_shared,
                thread_watchdog,
                rx,
                limits,
                fetch_wiring,
            );
        })
        .expect("spawn scene JS thread");

    JsRuntimeHandle {
        scene_hash,
        shared,
        tx,
        join: Mutex::new(Some(join)),
        watchdog,
        shutdown_join: std::time::Duration::from_millis(limits.js_shutdown_join_ms),
    }
}

pub(super) extern "C" fn near_heap_limit_cb(
    data: *mut c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    if !data.is_null() {
        let wd = unsafe { &*(data as *const Watchdog) };
        wd.fired.store(true, Ordering::SeqCst);
        if let Some(h) = wd.isolate.lock().as_ref() {
            h.terminate_execution();
        }
    }

    current_heap_limit + 32 * 1024 * 1024
}

type OomScene = (Arc<SharedState>, Arc<Watchdog>, String);

/// Scenes whose thread a V8 OOM parked; each keeps its isolate until restart.
#[derive(Default)]
struct ParkedScenes(Mutex<Vec<String>>);

impl ParkedScenes {
    fn add(&self, scene_hash: &str) {
        self.0.lock().push(scene_hash.to_string());
    }

    fn count(&self) -> usize {
        self.0.lock().len()
    }

    fn contains(&self, scene_hash: &str) -> bool {
        self.0.lock().iter().any(|s| s == scene_hash)
    }
}

static PARKED: ParkedScenes = ParkedScenes(Mutex::new(Vec::new()));

/// Counts a park the way the OOM handler does.
#[cfg(test)]
pub fn record_park_for_tests(scene_hash: &str) {
    PARKED.add(scene_hash);
}

/// Isolates parked by an OOM; they count against the scene cap.
pub fn parked_isolates() -> usize {
    PARKED.count()
}

/// Whether `scene_hash` ran out of memory before; it is not loaded again.
pub fn is_parked(scene_hash: &str) -> bool {
    PARKED.contains(scene_hash)
}

thread_local! {
    static OOM_SCENE: std::cell::RefCell<Option<OomScene>> = const { std::cell::RefCell::new(None) };
}

/// Routes a fatal V8 OOM on this thread to `scene` instead of the process.
pub(super) fn guard_oom(
    isolate: &mut v8::Isolate,
    shared: &Arc<SharedState>,
    watchdog: &Arc<Watchdog>,
    scene_hash: &str,
) {
    OOM_SCENE.with(|s| {
        *s.borrow_mut() = Some((Arc::clone(shared), Arc::clone(watchdog), scene_hash.into()))
    });
    isolate.set_oom_error_handler(scene_oom);
}

/// V8 aborts the process if this returns, so the scene's thread is parked for good.
unsafe extern "C" fn scene_oom(location: *const std::ffi::c_char, _details: &v8::OomDetails) {
    let location = if location.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(location) }
            .to_string_lossy()
            .into_owned()
    };
    OOM_SCENE.with(|s| match s.try_borrow().ok().as_deref().and_then(Option::as_ref) {
        Some((shared, watchdog, scene)) => {
            tracing::error!(%scene, %location, "scene isolate out of memory; parking its thread");
            PARKED.add(scene);
            shared.parked.store(true, Ordering::SeqCst);
            shared.running.store(false, Ordering::SeqCst);
            shared.slots.clear();
            watchdog.stopped.store(true, Ordering::SeqCst);
        }
        None => {
            tracing::error!(%location, "V8 out of memory off a scene thread; parking it");
            PARKED.add("");
        }
    });
    loop {
        std::thread::park();
    }
}

pub(super) fn finish(
    shared: &SharedState,
    watchdog: &Watchdog,
    wd_thread: Option<std::thread::JoinHandle<()>>,
    scene_hash: &str,
) {
    shared.running.store(false, Ordering::SeqCst);
    watchdog.stopped.store(true, Ordering::SeqCst);

    shared.slots.clear();

    *watchdog.isolate.lock() = None;
    if let Some(j) = wd_thread {
        let _ = j.join();
    }
    tracing::info!(scene = %scene_hash, "scene JS loop stopped");
}

impl Watchdog {
    pub(super) fn arm(&self, budget_ms: u64) {
        self.fired.store(false, Ordering::SeqCst);
        self.deadline_ms
            .store(self.now_ms().saturating_add(budget_ms), Ordering::SeqCst);
        self.tick_active.store(true, Ordering::SeqCst);
    }

    pub(super) fn disarm(&self) {
        self.tick_active.store(false, Ordering::SeqCst);
    }

    pub(super) fn was_terminated(&self) -> bool {
        let fired = self.fired.swap(false, Ordering::SeqCst);
        if fired && let Some(h) = self.isolate.lock().as_ref() {
            h.cancel_terminate_execution();
        }
        fired
    }
}

pub(super) fn watchdog_loop(watchdog: Arc<Watchdog>) {
    while !watchdog.stopped.load(Ordering::SeqCst) {
        if watchdog.tick_active.load(Ordering::SeqCst) {
            let now = watchdog.now_ms();
            let deadline = watchdog.deadline_ms.load(Ordering::SeqCst);
            if now >= deadline {
                watchdog.fired.store(true, Ordering::SeqCst);
                if let Some(h) = watchdog.isolate.lock().as_ref() {
                    h.terminate_execution();
                }

                while watchdog.tick_active.load(Ordering::SeqCst)
                    && !watchdog.stopped.load(Ordering::SeqCst)
                {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                continue;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_state::crdt::{CrdtMessage, decode_batch, encode_batch};

    fn put(entity: u32, comp: u32, ts: u32, data: &[u8]) -> CrdtMessage {
        CrdtMessage::Put {
            entity,
            component_id: comp,
            timestamp: ts,
            data: data.to_vec(),
        }
    }

    /// Connect a client the way `runtime::JsRuntime::on_client_open` does:
    /// take a slot, resolve its range once, and hand that range to the thread.
    fn connect(handle: &JsRuntimeHandle, tx: mpsc::Sender<Vec<u8>>) -> u32 {
        let index = handle.acquire_client_index();
        handle.shared.outbound.insert(index, tx);
        handle
            .shared
            .outbound_meters
            .insert(index, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let (start, size) = handle.shared.config.lock().range_for_client(index);
        handle
            .tx
            .send(Command::ClientOpen { index, start, size })
            .unwrap();
        index
    }

    /// The scene runs on its own thread with no signal back for engine state.
    fn wait_for<F: Fn() -> bool>(pred: F) -> bool {
        crate::shared::test_wait::until_blocking(pred)
    }

    #[test]
    fn scene_onstart_writes_to_engine() {
        let batch = encode_batch(&[put(600, 1, 1, &[42])]);
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            module.exports.onStart = async function () {{
              var bytes = new Uint8Array({:?});
              await EngineApi.crdtSendToRenderer({{ data: bytes }});
            }};
            registerScene(
              {{ reservedLocalEntities: 512, networkEntitiesLimit: {{ serverLimit: 512, clientLimit: 512 }} }},
              function (ev) {{}}
            );
            "#,
            batch
        );
        let handle = spawn(
            "test-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );
        let ok = wait_for(|| {
            let eng = handle.shared.engine.lock();
            eng.component_count() == 1
        });
        assert!(ok, "scene onStart should have written one component");
        let snap = handle.shared.snapshot.lock().clone();
        assert_eq!(decode_batch(&snap), vec![put(600, 1, 1, &[42])]);
    }

    #[test]
    fn restricted_actions_stubs_resolve() {
        let batch = encode_batch(&[put(601, 1, 1, &[7])]);
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            var RestrictedActions = require('~system/RestrictedActions');
            module.exports.onStart = async function () {{
              await RestrictedActions.triggerEmote({{ predefinedEmote: 'wave' }});
              await RestrictedActions.stopEmote({{}});
              var bytes = new Uint8Array({:?});
              await EngineApi.crdtSendToRenderer({{ data: bytes }});
            }};
            registerScene(
              {{ reservedLocalEntities: 512, networkEntitiesLimit: {{ serverLimit: 512, clientLimit: 512 }} }},
              function (ev) {{}}
            );
            "#,
            batch
        );
        let handle = spawn(
            "restricted-actions-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );
        let ok = wait_for(|| {
            let eng = handle.shared.engine.lock();
            eng.component_count() == 1
        });
        assert!(
            ok,
            "restricted-action stubs (incl. stopEmote) must resolve so onStart reaches the write"
        );
    }

    #[test]
    fn scene_relays_client_messages_via_observer() {
        let js = r#"
            var clients = {};
            registerScene(
              { reservedLocalEntities: 512, networkEntitiesLimit: { serverLimit: 512, clientLimit: 512 } },
              function (ev) {
                if (ev.type === 'open') { clients[ev.clientId] = ev.client; }
                if (ev.type === 'close') { delete clients[ev.clientId]; }
              }
            );
            module.exports.onUpdate = async function (dt) {
              for (var id in clients) {
                var msgs = clients[id].getMessages();
                for (var i = 0; i < msgs.length; i++) {
                  // Echo the exact bytes back to the same client.
                  clients[id].sendCrdtMessage(msgs[i]);
                }
              }
            };
        "#
        .to_string();

        let handle = spawn(
            "echo-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let index = connect(&handle, tx);

        let body = encode_batch(&[put(1100, 1, 7, &[1, 2, 3])]);
        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: body.clone(),
            })
            .unwrap();

        let frame = wait_for_recv(&mut rx);
        assert!(frame.is_some(), "expected an echoed Crdt frame");
        let frame = frame.unwrap();

        assert_eq!(
            frame[0],
            crate::scene_state::protocol::MessageType::Crdt as u8
        );
        assert_eq!(decode_batch(&frame[1..]), vec![put(1100, 1, 7, &[1, 2, 3])]);
    }

    #[test]
    fn client_close_reclaims_range() {
        let js = r#"
            registerScene(
              { reservedLocalEntities: 512, networkEntitiesLimit: { serverLimit: 512, clientLimit: 512 } },
              function (ev) {}
            );
            module.exports.onStart = async function () {};
        "#
        .to_string();
        let handle = spawn(
            "close-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let (tx, _rx) = mpsc::channel::<Vec<u8>>(64);
        let index = connect(&handle, tx);

        let body = encode_batch(&[put(1100, 1, 1, &[1])]);
        handle.tx.send(Command::ClientCrdt { index, body }).unwrap();

        handle.shared.engine.lock().apply(&put(1100, 1, 1, &[1]));
        assert!(handle.shared.engine.lock().component_count() >= 1);

        handle.tx.send(Command::ClientClose { index }).unwrap();
        let reclaimed = wait_for(|| handle.shared.engine.lock().component_count() == 0);
        assert!(reclaimed, "close should reclaim the client's network range");
    }

    /// The next frame the scene sends this client; `None` only past the hang guard.
    fn wait_for_recv(rx: &mut mpsc::Receiver<Vec<u8>>) -> Option<Vec<u8>> {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(crate::shared::test_wait::HANG_GUARD, rx.recv()).await
            })
            .ok()
            .flatten()
    }

    #[test]
    fn infinite_onupdate_is_terminated_and_shutdown_does_not_hang() {
        let js = r#"
            registerScene(
              { reservedLocalEntities: 512, networkEntitiesLimit: { serverLimit: 512, clientLimit: 512 } },
              function (ev) {}
            );
            module.exports.onUpdate = function (dt) {
              // Spin forever -- must be force-terminated by the watchdog.
              while (true) {}
            };
        "#
        .to_string();
        let limits = RuntimeLimits {
            js_tick_budget_ms: 100,
            js_shutdown_join_ms: 2000,
            ..RuntimeLimits::default()
        };
        let handle = spawn(
            "spin-scene".into(),
            js,
            "dcl-test".into(),
            limits,
            Vec::new(),
            None,
        );

        let stopped = wait_for(|| !handle.shared.running.load(Ordering::SeqCst));
        assert!(
            stopped,
            "watchdog should have terminated the infinite onUpdate"
        );

        // Shutdown either joins the stopped scene thread or detaches it at the join budget;
        // both return and release the handle. A shutdown that blocks only hits the hang guard.
        let (returned, shut) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            handle.shutdown();
            let _ = returned.send(handle.join.lock().is_none());
        });
        let released = shut
            .recv_timeout(crate::shared::test_wait::HANG_GUARD)
            .expect("shutdown() must not block on a wedged scene");
        assert!(
            released,
            "shutdown() returned still holding the scene thread"
        );
    }

    #[test]
    fn top_level_var_does_not_clobber_globals() {
        let batch = encode_batch(&[put(700, 1, 1, &[7])]);
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            var DEBUG_NETWORK_MESSAGES = () => globalThis.DEBUG_NETWORK_MESSAGES ?? false;
            globalThis.DEBUG_NETWORK_MESSAGES = true;
            module.exports.onStart = async function () {{
              if (DEBUG_NETWORK_MESSAGES() !== true) {{ throw new Error('global clobbered'); }}
              await EngineApi.crdtSendToRenderer({{ data: new Uint8Array({batch:?}) }});
            }};
            "#
        );
        let handle = spawn(
            "wrapper-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );
        let ok = wait_for(|| handle.shared.engine.lock().component_count() == 1);
        assert!(
            ok,
            "top-level var collided with a global; the CJS wrapper must isolate it"
        );
        let snap = handle.shared.snapshot.lock().clone();
        assert_eq!(decode_batch(&snap), vec![put(700, 1, 1, &[7])]);
    }

    #[test]
    fn set_immediate_queued_during_drain_runs_next_drain() {
        let first = encode_batch(&[put(600, 1, 1, &[1])]);
        let second = encode_batch(&[put(601, 1, 1, &[2])]);
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            setImmediate(function () {{
              EngineApi.crdtSendToRenderer({{ data: new Uint8Array({first:?}) }});
              setImmediate(function () {{
                EngineApi.crdtSendToRenderer({{ data: new Uint8Array({second:?}) }});
              }});
            }});
            module.exports.onUpdate = function (dt) {{}};
            "#
        );
        let handle = spawn(
            "requeue-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );
        let ok = wait_for(|| handle.shared.engine.lock().component_count() == 2);
        assert!(
            ok,
            "a setImmediate queued during the drain must run on the next drain, not never"
        );
    }

    #[test]
    fn onupdate_throw_skips_frame_and_scene_continues() {
        let batch = encode_batch(&[put(800, 1, 1, &[8])]);
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            var ticks = 0;
            module.exports.onUpdate = function (dt) {{
              ticks++;
              if (ticks === 1) {{ throw new Error('frame 1 fails'); }}
              if (ticks === 2) {{ EngineApi.crdtSendToRenderer({{ data: new Uint8Array({batch:?}) }}); }}
            }};
            "#
        );
        let handle = spawn(
            "throwy-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );
        let ok = wait_for(|| handle.shared.engine.lock().component_count() == 1);
        assert!(
            ok,
            "an onUpdate throw must skip the frame, not kill the loop"
        );
        assert!(handle.shared.running.load(Ordering::SeqCst));
    }

    #[test]
    fn onupdate_consecutive_throws_tear_scene_down() {
        let js = r#"
            module.exports.onUpdate = function (dt) { throw new Error('always'); };
        "#
        .to_string();
        let limits = RuntimeLimits {
            js_update_failure_cap: 3,
            ..RuntimeLimits::default()
        };
        let handle = spawn(
            "doomed-scene".into(),
            js,
            "dcl-test".into(),
            limits,
            Vec::new(),
            None,
        );
        let stopped = wait_for(|| !handle.shared.running.load(Ordering::SeqCst));
        assert!(
            stopped,
            "hitting the consecutive-failure cap must tear the scene down"
        );
    }

    #[test]
    fn out_of_range_client_ops_never_reach_the_scene() {
        let js = r#"
            var clients = {};
            registerScene(
              { reservedLocalEntities: 512, networkEntitiesLimit: { serverLimit: 512, clientLimit: 512 } },
              function (ev) {
                if (ev.type === 'open') { clients[ev.clientId] = ev.client; }
                if (ev.type === 'close') { delete clients[ev.clientId]; }
              }
            );
            module.exports.onUpdate = function (dt) {
              for (var id in clients) {
                var msgs = clients[id].getMessages();
                for (var i = 0; i < msgs.length; i++) {
                  clients[id].sendCrdtMessage(msgs[i]);
                }
              }
            };
        "#
        .to_string();
        let handle = spawn(
            "range-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let index = connect(&handle, tx);

        let out_of_range = encode_batch(&[
            put(5000, 1, 7, &[9]),
            CrdtMessage::DeleteEntity {
                entity: 4_000_000_000,
            },
        ]);
        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: out_of_range,
            })
            .unwrap();
        let in_range = encode_batch(&[put(1100, 1, 7, &[1])]);
        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: in_range,
            })
            .unwrap();

        let frame = wait_for_recv(&mut rx).expect("in-range op must still be echoed");
        assert_eq!(decode_batch(&frame[1..]), vec![put(1100, 1, 7, &[1])]);

        let snap = handle.shared.snapshot.lock().clone();
        assert_eq!(decode_batch(&snap), vec![put(1100, 1, 7, &[1])]);
        assert_eq!(handle.shared.engine.lock().deleted_count(), 0);
    }

    #[test]
    fn crdt_cap_rejects_new_cells() {
        use crate::scene_state::crdt::{ApplyResult, CrdtEngine};
        let mut e = CrdtEngine::with_cap(2);
        assert_eq!(e.apply(&put(1, 1, 1, b"a")), ApplyResult::Applied);
        assert_eq!(e.apply(&put(1, 2, 1, b"b")), ApplyResult::Applied);

        assert_eq!(e.apply(&put(1, 3, 1, b"c")), ApplyResult::Ignored);

        assert_eq!(e.apply(&put(1, 1, 2, b"a2")), ApplyResult::Applied);
        assert_eq!(e.component_count(), 2);
    }

    const CFG_512: &str = "{ reservedLocalEntities: 512, networkEntitiesLimit: { serverLimit: 512, clientLimit: 512 } }";

    /// Live PUTs in the engine snapshot, ignoring whatever tombstone
    /// bookkeeping `crdt.rs` carries alongside them.
    fn live_puts(handle: &JsRuntimeHandle) -> Vec<CrdtMessage> {
        let snap = handle.shared.snapshot.lock().clone();
        decode_batch(&snap)
            .into_iter()
            .filter(|m| matches!(m, CrdtMessage::Put { .. }))
            .collect()
    }

    /// D1. `EngineApi.crdtSendToRenderer` decoded and applied whatever it was
    /// given. Entity 1 is PLAYER in *every* client's local engine, so a network
    /// write there moves every avatar -- the thing `RestrictedActions` is
    /// stubbed out on the server to prevent.
    #[test]
    fn scene_cannot_author_renderer_local_entities() {
        let batch = encode_batch(&[put(1, 1, 1, &[9]), put(2, 1, 1, &[9]), put(600, 1, 1, &[7])]);
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            module.exports.onStart = async function () {{
              await EngineApi.crdtSendToRenderer({{ data: new Uint8Array({batch:?}) }});
            }};
            registerScene({CFG_512}, function (ev) {{}});
            "#
        );
        let handle = spawn(
            "reserved-write-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let ok = wait_for(|| !live_puts(&handle).is_empty());
        assert!(ok, "the legitimate write should have landed");
        assert_eq!(
            live_puts(&handle),
            vec![put(600, 1, 1, &[7])],
            "writes to the renderer-local block must never reach the shared state"
        );
    }

    /// D1. The same hole through the other unfiltered apply boundary:
    /// `client.sendCrdtMessage`. The forged record must reach neither the
    /// engine nor the wire.
    #[test]
    fn scene_cannot_forge_renderer_local_writes_at_a_client() {
        let forged = encode_batch(&[put(1, 1, 1, &[9]), put(600, 1, 1, &[7])]);
        let js = format!(
            r#"
            registerScene({CFG_512}, function (ev) {{
              if (ev.type === 'open') {{
                ev.client.sendCrdtMessage(new Uint8Array({forged:?}));
              }}
            }});
            module.exports.onUpdate = function (dt) {{}};
            "#
        );
        let handle = spawn(
            "forge-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let _index = connect(&handle, tx);

        let frame = wait_for_recv(&mut rx).expect("the admitted record must still be sent");
        assert_eq!(
            decode_batch(&frame[1..]),
            vec![put(600, 1, 1, &[7])],
            "the forged PLAYER write must not reach the client either"
        );
        assert_eq!(live_puts(&handle), vec![put(600, 1, 1, &[7])]);
    }

    /// D5. `registerScene(cfg, 42)` sets no observer. Reclaim is a server
    /// invariant and must run anyway -- it used to return early, leaking the
    /// departed client's entities for the lifetime of the scene.
    #[test]
    fn reclaim_runs_with_a_non_function_observer() {
        let js = format!(
            r#"
            registerScene({CFG_512}, 42);
            module.exports.onUpdate = function (dt) {{}};
            "#
        );
        assert_reclaims_on_close("noobs-scene", js);
    }

    /// D5. Same, for a scene that never calls `registerScene` at all.
    #[test]
    fn reclaim_runs_without_register_scene() {
        assert_reclaims_on_close(
            "unregistered-scene",
            "module.exports.onUpdate = function (dt) {};".to_string(),
        );
    }

    fn assert_reclaims_on_close(name: &str, js: String) {
        let handle = spawn(
            name.into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(64);
        let index = connect(&handle, tx);

        handle.shared.engine.lock().apply(&put(1100, 7, 1, &[1]));
        assert_eq!(handle.shared.engine.lock().component_count(), 1);

        handle.tx.send(Command::ClientClose { index }).unwrap();
        assert!(
            wait_for(|| handle.shared.engine.lock().component_count() == 0),
            "a departed client's range must be reclaimed whether or not the \
             scene ever registered an observer"
        );
    }

    /// D2. Slots are a bounded resource and come back on disconnect, so
    /// reconnects cannot walk past the last representable range.
    #[test]
    fn js_runtime_recycles_client_slots() {
        let js = format!(
            r#"
            registerScene({CFG_512}, function (ev) {{}});
            module.exports.onUpdate = function (dt) {{}};
            "#
        );
        let handle = spawn(
            "recycle-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let (tx, _rx) = mpsc::channel::<Vec<u8>>(64);
        let first = connect(&handle, tx);
        assert_eq!(first, 0);

        handle
            .tx
            .send(Command::ClientClose { index: first })
            .unwrap();
        assert!(
            wait_for(|| handle.shared.slots.live() == 0),
            "the slot must be released once the range has been reclaimed"
        );

        let (tx2, _rx2) = mpsc::channel::<Vec<u8>>(64);
        assert_eq!(
            connect(&handle, tx2),
            0,
            "a reconnect must reuse the freed slot"
        );
    }

    /// D3. `registerScene` is reachable from `onUpdate`. Re-partitioning the
    /// entity space while clients hold ranges derived from the old partition
    /// gives two live clients the same ids and can slide a client's window down
    /// over ROOT / PLAYER / CAMERA.
    #[test]
    fn mid_session_register_scene_cannot_move_the_partition() {
        let js = format!(
            r#"
            var clients = {{}};
            var open = false;
            registerScene({CFG_512}, function (ev) {{
              if (ev.type === 'open') {{ clients[ev.clientId] = ev.client; open = true; }}
              if (ev.type === 'close') {{ delete clients[ev.clientId]; }}
            }});
            module.exports.onUpdate = function (dt) {{
              if (open) {{
                // hostile: hand every client the whole id space, reserved block included
                registerScene(
                  {{ reservedLocalEntities: 0, networkEntitiesLimit: {{ serverLimit: 0, clientLimit: 65536 }} }},
                  function (ev) {{}}
                );
              }}
              for (var id in clients) {{
                var msgs = clients[id].getMessages();
                for (var i = 0; i < msgs.length; i++) {{ clients[id].sendCrdtMessage(msgs[i]); }}
              }}
            }};
            "#
        );
        let handle = spawn(
            "hijack-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let index = connect(&handle, tx);
        // The probe's echo comes from an onUpdate that already saw the open, so the
        // hostile registerScene above ran before the config is read.
        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: encode_batch(&[put(1100, 7, 1, &[1])]),
            })
            .unwrap();
        let frame = wait_for_recv(&mut rx).expect("the probe must be echoed");
        assert_eq!(decode_batch(&frame[1..]), vec![put(1100, 7, 1, &[1])]);

        let cfg = *handle.shared.config.lock();
        assert_eq!(
            (
                cfg.reserved_local_entities,
                cfg.server_network_entities_limit,
                cfg.client_network_entities_limit
            ),
            (512, 512, 512),
            "the transport config must freeze once a client holds a range from it"
        );

        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: encode_batch(&[put(0, 7, 1, &[9]), put(600, 7, 1, &[9])]),
            })
            .unwrap();
        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: encode_batch(&[put(1100, 7, 2, &[2])]),
            })
            .unwrap();

        let frame = wait_for_recv(&mut rx).expect("the in-range op must be echoed");
        assert_eq!(decode_batch(&frame[1..]), vec![put(1100, 7, 2, &[2])]);
        assert_eq!(live_puts(&handle), vec![put(1100, 7, 2, &[2])]);
    }

    /// D3, the other half: the *reclaim* used to be recomputed at close time
    /// from the current config rather than from the range the client was
    /// actually given, so a config change made close either leak the departed
    /// client's entities or wipe a live neighbour's. Both the authority check
    /// and the reclaim must follow the handed range.
    #[test]
    fn client_is_judged_and_reclaimed_by_the_range_it_was_handed() {
        let js = format!(
            r#"
            var clients = {{}};
            registerScene({CFG_512}, function (ev) {{
              if (ev.type === 'open') {{ clients[ev.clientId] = ev.client; }}
              if (ev.type === 'close') {{ delete clients[ev.clientId]; }}
            }});
            module.exports.onUpdate = function (dt) {{
              for (var id in clients) {{
                var msgs = clients[id].getMessages();
                for (var i = 0; i < msgs.length; i++) {{ clients[id].sendCrdtMessage(msgs[i]); }}
              }}
            }};
            "#
        );
        let handle = spawn(
            "handed-range-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let index = handle.acquire_client_index();
        handle.shared.outbound.insert(index, tx);
        handle
            .tx
            .send(Command::ClientOpen {
                index,
                start: 2048,
                size: 512,
            })
            .unwrap();

        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: encode_batch(&[put(1100, 7, 1, &[9])]),
            })
            .unwrap();
        let mine = put(2100, 7, 1, &[1]);
        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: encode_batch(std::slice::from_ref(&mine)),
            })
            .unwrap();

        let frame = wait_for_recv(&mut rx).expect("the handed-range op must be echoed");
        assert_eq!(
            decode_batch(&frame[1..]),
            vec![mine.clone()],
            "the config's window for index 0 must not admit anything for this client"
        );

        let neighbour = put(1100, 7, 1, &[5]);
        handle.shared.engine.lock().apply(&neighbour);
        assert!(wait_for(
            || handle.shared.engine.lock().component_count() == 2
        ));

        handle.tx.send(Command::ClientClose { index }).unwrap();
        assert!(
            wait_for(|| handle.shared.engine.lock().component_count() == 1),
            "close must reclaim the handed range"
        );
        assert_eq!(
            live_puts(&handle),
            vec![neighbour],
            "close reclaimed the config's window instead of the handed one"
        );
    }

    /// D6. A client's in-range transform write whose payload names a parent in
    /// another live client's range grafts its object onto the neighbour's.
    #[test]
    fn client_cannot_reparent_onto_another_clients_entity() {
        use crate::scene_state::runtime::{
            TRANSFORM_COMPONENT_ID, TRANSFORM_PARENT_OFFSET, TRANSFORM_PAYLOAD_LEN,
        };

        let js = format!(
            r#"
            var clients = {{}};
            registerScene({CFG_512}, function (ev) {{
              if (ev.type === 'open') {{ clients[ev.clientId] = ev.client; }}
              if (ev.type === 'close') {{ delete clients[ev.clientId]; }}
            }});
            module.exports.onUpdate = function (dt) {{
              for (var id in clients) {{
                var msgs = clients[id].getMessages();
                for (var i = 0; i < msgs.length; i++) {{ clients[id].sendCrdtMessage(msgs[i]); }}
              }}
            }};
            "#
        );
        let handle = spawn(
            "graph-scene".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );

        let (tx_a, mut rx_a) = mpsc::channel::<Vec<u8>>(64);
        let (tx_b, _rx_b) = mpsc::channel::<Vec<u8>>(64);
        let a = connect(&handle, tx_a);
        let _b = connect(&handle, tx_b);

        let transform = |parent: u32| {
            let mut data = vec![0u8; TRANSFORM_PAYLOAD_LEN];
            data[TRANSFORM_PARENT_OFFSET..TRANSFORM_PARENT_OFFSET + 4]
                .copy_from_slice(&parent.to_le_bytes());
            data
        };

        let graft = put(1100, TRANSFORM_COMPONENT_ID, 1, &transform(1600));
        let ok = put(1101, TRANSFORM_COMPONENT_ID, 1, &transform(0));
        handle
            .tx
            .send(Command::ClientCrdt {
                index: a,
                body: encode_batch(&[graft]),
            })
            .unwrap();
        handle
            .tx
            .send(Command::ClientCrdt {
                index: a,
                body: encode_batch(std::slice::from_ref(&ok)),
            })
            .unwrap();

        let frame = wait_for_recv(&mut rx_a).expect("the ROOT-parented write must be echoed");
        assert_eq!(
            decode_batch(&frame[1..]),
            vec![ok.clone()],
            "the cross-client reparent must never reach the scene"
        );
        assert_eq!(live_puts(&handle), vec![ok]);
    }

    #[test]
    fn a_spinning_set_immediate_is_terminated() {
        let js = r#"
            setImmediate(function () { for (;;) {} });
            module.exports.onUpdate = function (dt) {};
        "#
        .to_string();
        let limits = RuntimeLimits {
            js_tick_budget_ms: 100,
            ..RuntimeLimits::default()
        };
        let handle = spawn(
            "immediate-spin".into(),
            js,
            "dcl-test".into(),
            limits,
            Vec::new(),
            None,
        );
        assert!(
            wait_for(|| !handle.shared.running.load(Ordering::SeqCst)),
            "the watchdog must stop a setImmediate callback that never returns"
        );
    }

    /// Runs `body` in `onStart` and reports whether it set `ok`, by writing
    /// entity 600 only when it did.
    fn scene_reports(name: &str, body: &str, limits: RuntimeLimits) -> bool {
        let batch = encode_batch(&[put(600, 1, 1, &[1])]);
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            module.exports.onStart = function () {{
              var ok = false;
              {body}
              if (ok) {{ EngineApi.crdtSendToRenderer({{ data: new Uint8Array({batch:?}) }}); }}
            }};
            "#
        );
        let handle = spawn(name.into(), js, "dcl-test".into(), limits, Vec::new(), None);
        wait_for(|| handle.shared.engine.lock().component_count() == 1)
    }

    #[test]
    fn array_buffers_are_held_to_the_isolate_budget() {
        let limits = RuntimeLimits {
            js_array_buffer_limit_mb: 8,
            ..RuntimeLimits::default()
        };
        assert!(
            scene_reports(
                "ab-within",
                "var keep = new Uint8Array(4 << 20); keep.fill(1); ok = keep[0] === 1;",
                limits,
            ),
            "an allocation inside the budget succeeds"
        );
        let js = r#"
            var keep = [];
            module.exports.onUpdate = function () { keep.push(new Uint8Array(4 << 20)); };
        "#;
        let handle = spawn(
            "ab-over".into(),
            js.to_string(),
            "dcl-test".into(),
            limits,
            Vec::new(),
            None,
        );
        assert!(
            wait_for(|| !handle.shared.running.load(Ordering::SeqCst)),
            "a scene that keeps allocating past the budget is stopped"
        );
    }

    #[test]
    fn host_buffers_over_the_crdt_budget_throw() {
        let limits = RuntimeLimits {
            crdt_max_bytes: 64,
            ..RuntimeLimits::default()
        };
        assert!(
            scene_reports(
                "crdt-within",
                "updateCRDTState(new Uint8Array(64)); ok = true;",
                limits,
            ),
            "a buffer at the cap is accepted"
        );
        assert!(
            scene_reports(
                "crdt-over",
                "try { updateCRDTState(new Uint8Array(65)); } catch (e) { ok = e instanceof RangeError; } \
                 try { EngineApi.crdtSendToRenderer({ data: new Uint8Array(65) }); ok = false; } \
                 catch (e) { ok = ok && e instanceof RangeError; }",
                limits,
            ),
            "a buffer over the cap throws"
        );
    }

    #[test]
    fn array_buffer_budget_returns_freed_host_buffers() {
        let budget = super::super::array_buffers::ArrayBufferBudget::new(1024);
        ensure_v8_initialized();
        let mut isolate = v8::Isolate::new(
            v8::CreateParams::default()
                .array_buffer_allocator(super::super::array_buffers::allocator(&budget)),
        );
        {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            assert!(super::super::array_buffers::uint8array(scope, &budget, &[1; 600]).is_some());
            assert_eq!(budget.used(), 600);
            assert!(
                super::super::array_buffers::uint8array(scope, &budget, &[1; 600]).is_none(),
                "a host buffer past the budget is refused"
            );
        }
        drop(isolate);
        assert_eq!(budget.used(), 0, "dropping the isolate frees its buffers");
    }

    #[test]
    fn a_replaced_reflect_cannot_reach_the_original_array_buffer() {
        assert!(
            scene_reports(
                "harden-reflect",
                "var R = Reflect, AB0; \
                 Reflect = { construct: function (t, a, n) { AB0 = t; return R.construct(t, a, n); } }; \
                 var plain = new ArrayBuffer(1); Reflect = R; \
                 ok = AB0 === undefined && plain.byteLength === 1 \
                   && new ArrayBuffer(8, { maxByteLength: 64 }).resizable === false \
                   && Object.getOwnPropertyDescriptor(Object.getPrototypeOf(plain), 'constructor').writable === false;",
                RuntimeLimits::default(),
            ),
            "the proxy trap uses its own Reflect.construct"
        );
    }

    #[test]
    fn buffers_outside_the_allocator_are_not_constructible() {
        assert!(
            scene_reports(
                "harden",
                "ok = typeof WebAssembly === 'undefined' \
                   && typeof SharedArrayBuffer === 'undefined' \
                   && typeof ArrayBuffer.prototype.resize === 'undefined' \
                   && new ArrayBuffer(8, { maxByteLength: 1 << 30 }).resizable === false \
                   && new (new Uint8Array(4).buffer.constructor)(8, { maxByteLength: 16 }).resizable === false \
                   && new Uint8Array(4).buffer instanceof ArrayBuffer \
                   && ArrayBuffer.isView(new Uint8Array(1)) \
                   && new Uint8Array(new ArrayBuffer(16)).length === 16;",
                RuntimeLimits::default(),
            ),
            "resizable, shared and wasm memory are gone while plain buffers work"
        );
    }

    /// Forwards what each client sends to every client, and floods each
    /// client with `FILL` every tick.
    fn relay_flood(fill: &[u8]) -> String {
        format!(
            r#"
            var clients = {{}};
            var FILL = new Uint8Array({fill:?});
            registerScene({CFG_512}, function (ev) {{
              if (ev.type === 'open') {{ clients[ev.clientId] = ev.client; }}
              if (ev.type === 'close') {{ delete clients[ev.clientId]; }}
            }});
            module.exports.onUpdate = function (dt) {{
              for (var id in clients) {{
                var msgs = clients[id].getMessages();
                for (var i = 0; i < msgs.length; i++) {{
                  for (var to in clients) {{ clients[to].sendCrdtMessage(msgs[i]); }}
                }}
              }}
              for (var to in clients) {{
                for (var k = 0; k < 8; k++) {{ clients[to].sendCrdtMessage(FILL); }}
              }}
            }};
            "#
        )
    }

    #[test]
    fn a_reclaim_evicted_by_a_flood_does_not_corrupt_the_outbound_count() {
        let fill = encode_batch(&[put(700, 1, 1, &[1])]);
        let limits = RuntimeLimits {
            client_outbound_max: 4,
            ..RuntimeLimits::default()
        };
        let handle = spawn(
            "reclaim-flood".into(),
            relay_flood(&fill),
            "dcl-test".into(),
            limits,
            Vec::new(),
            None,
        );
        let (tx_a, _rx_a) = mpsc::channel::<Vec<u8>>(1024);
        let (tx_b, _rx_b) = mpsc::channel::<Vec<u8>>(1024);
        let a = connect(&handle, tx_a);
        let _b = connect(&handle, tx_b);
        handle
            .tx
            .send(Command::ClientCrdt {
                index: a,
                body: encode_batch(&[put(1100, 1, 1, &[5])]),
            })
            .unwrap();
        assert!(wait_for(
            || handle.shared.engine.lock().component_count() >= 2
        ));
        handle.tx.send(Command::ClientClose { index: a }).unwrap();
        handle.shared.outbound.remove(&a);
        assert!(wait_for(|| handle.shared.slots.live() == 1));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            handle.shared.running.load(Ordering::SeqCst),
            "the scene keeps running after the reclaim is evicted"
        );
    }

    #[test]
    fn unwritten_socket_bytes_stay_within_the_outbound_budget() {
        let fill = encode_batch(&[put(700, 1, 1, &[1; 100])]);
        let limits = RuntimeLimits {
            client_outbound_max_bytes: 1000,
            ..RuntimeLimits::default()
        };
        let handle = spawn(
            "outbound-meter".into(),
            relay_flood(&fill),
            "dcl-test".into(),
            limits,
            Vec::new(),
            None,
        );
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1024);
        let index = connect(&handle, tx);
        let meter = Arc::clone(&handle.shared.outbound_meters.get(&index).unwrap());
        assert!(wait_for(|| meter.load(Ordering::SeqCst) > 0));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut queued = 0;
        while let Ok(frame) = rx.try_recv() {
            queued += frame.len();
        }
        assert!(queued > 0);
        assert!(
            queued <= 1000 + fill.len() + 1,
            "{queued} bytes handed to a socket that never writes"
        );
    }

    #[test]
    fn frames_sent_before_a_late_register_scene_reach_the_scene() {
        let js = format!(
            r#"
            var clients = {{}};
            var ticks = 0;
            module.exports.onUpdate = function (dt) {{
              ticks++;
              if (ticks === 15) {{
                registerScene({CFG_512}, function (ev) {{
                  if (ev.type === 'open') {{ clients[ev.clientId] = ev.client; }}
                }});
              }}
              for (var id in clients) {{
                var msgs = clients[id].getMessages();
                for (var i = 0; i < msgs.length; i++) {{ clients[id].sendCrdtMessage(msgs[i]); }}
              }}
            }};
            "#
        );
        let handle = spawn(
            "late-register".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let index = connect(&handle, tx);
        let early = put(1100, 1, 1, &[9]);
        handle
            .tx
            .send(Command::ClientCrdt {
                index,
                body: encode_batch(std::slice::from_ref(&early)),
            })
            .unwrap();
        let frame = wait_for_recv(&mut rx).expect("the early frame is delivered once registered");
        assert_eq!(decode_batch(&frame[1..]), vec![early]);
    }

    #[test]
    fn element_setters_cannot_reenter_get_messages() {
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            var clients = {{}};
            registerScene({CFG_512}, function (ev) {{
              if (ev.type === 'open') {{ clients[ev.clientId] = ev.client; }}
            }});
            var armed = false;
            module.exports.onUpdate = function (dt) {{
              for (var id in clients) {{
                var c = clients[id];
                if (!armed) {{
                  armed = true;
                  Object.defineProperty(Array.prototype, '0', {{
                    set: function (v) {{ c.getMessages(); EngineApi.crdtGetState(); }},
                    configurable: true
                  }});
                }}
                var msgs = c.getMessages();
                EngineApi.crdtGetState();
                for (var i = 0; i < msgs.length; i++) {{ c.sendCrdtMessage(msgs[i]); }}
              }}
            }};
            "#
        );
        let handle = spawn(
            "setter-reentry".into(),
            js,
            "dcl-test".into(),
            RuntimeLimits::default(),
            Vec::new(),
            None,
        );
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let index = connect(&handle, tx);
        let sent = put(1100, 1, 1, &[3]);
        for _ in 0..3 {
            handle
                .tx
                .send(Command::ClientCrdt {
                    index,
                    body: encode_batch(std::slice::from_ref(&sent)),
                })
                .unwrap();
        }
        let frame = wait_for_recv(&mut rx).expect("messages still reach the scene");
        assert_eq!(decode_batch(&frame[1..]), vec![sent]);
        assert!(handle.shared.running.load(Ordering::SeqCst));
    }

    /// Runs `body` in `onStart`: `(leaked, stopped)`, where `leaked` means an
    /// allocation past the budget succeeded and `stopped` that the scene was
    /// terminated rather than finishing `onStart`.
    fn scene_leaks_with(name: &str, body: &str, limits: RuntimeLimits) -> (bool, bool) {
        let mark = |e: u32| encode_batch(&[put(e, 1, 1, &[1])]);
        let (started, leaked, finished) = (mark(601), mark(600), mark(602));
        let js = format!(
            r#"
            var EngineApi = require('~system/EngineApi');
            function mark(b) {{ EngineApi.crdtSendToRenderer({{ data: new Uint8Array(b) }}); }}
            module.exports.onStart = function () {{
              mark({started:?});
              var leaked = false;
              try {{ {body} }} catch (e) {{}}
              if (leaked) {{ mark({leaked:?}); }}
              mark({finished:?});
            }};
            module.exports.onUpdate = function (dt) {{}};
            "#
        );
        let handle = spawn(name.into(), js, "dcl-test".into(), limits, Vec::new(), None);
        let has = |e: u32| {
            decode_batch(&handle.shared.snapshot.lock())
                .iter()
                .any(|m| m.entity() == e)
        };
        assert!(
            wait_for(|| has(601) && (has(602) || !handle.shared.running.load(Ordering::SeqCst))),
            "{name}: onStart never ran"
        );
        (has(600), !has(602))
    }

    fn scene_leaks(name: &str, body: &str, limits: RuntimeLimits) -> bool {
        scene_leaks_with(name, body, limits).0
    }

    #[test]
    fn no_buffer_path_allocates_past_the_budget() {
        let limits = RuntimeLimits {
            js_array_buffer_limit_mb: 1,
            ..RuntimeLimits::default()
        };
        assert!(
            scene_leaks(
                "ab-control",
                "new ArrayBuffer(1 << 19); leaked = true;",
                limits
            ),
            "control: an allocation inside the budget succeeds"
        );
        for (name, body) in [
            ("ab-ctor", "new ArrayBuffer(2 << 20); leaked = true;"),
            (
                "ab-transfer",
                "new ArrayBuffer(1).transfer(2 << 20); leaked = true;",
            ),
            (
                "ab-small",
                "var keep = [new Uint8Array((1 << 20) - 4096)]; \
                 for (var j = 0; j < 1000; j++) keep.push(new ArrayBuffer(64)); leaked = true;",
            ),
        ] {
            let (leaked, stopped) = scene_leaks_with(name, body, limits);
            assert!(!leaked, "{name} allocated past the budget");
            assert!(stopped, "{name}: going over the budget stops the scene");
            assert!(
                !super::is_parked(name),
                "{name}: the stop is the heap-limit termination, not an OOM park"
            );
        }
    }

    #[test]
    fn a_coerced_length_never_runs_scene_code_in_the_native_constructor() {
        assert!(
            scene_reports(
                "harden-coerce",
                "function native(name) { return new Error().stack.indexOf('new ' + name) >= 0; } \
                 var inDataView, inArrayBuffer; \
                 new DataView(new ArrayBuffer(8), { valueOf: function () { inDataView = native('DataView'); return 0; } }); \
                 var b = new ArrayBuffer({ valueOf: function () { inArrayBuffer = native('ArrayBuffer'); return 8; } }); \
                 ok = inDataView === true && inArrayBuffer === false && b.byteLength === 8;",
                RuntimeLimits::default(),
            ),
            "the length is converted by the trap, outside any native constructor frame"
        );
    }

    #[test]
    fn parked_scenes_are_counted_and_remembered() {
        let parked = ParkedScenes::default();
        assert_eq!((parked.count(), parked.contains("a")), (0, false));
        parked.add("a");
        parked.add("");
        assert_eq!((parked.count(), parked.contains("a")), (2, true));
    }
}

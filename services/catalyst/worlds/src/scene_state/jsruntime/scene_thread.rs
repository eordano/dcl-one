use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::scene_state::crdt::{CrdtEngine, decode_batch, decode_client_batch, encode_batch};
use crate::scene_state::runtime::{
    Authority, AuthorityGuard, ClientSlots, InboundBudget, RuntimeLimits, ServerTransportConfig,
};

use super::array_buffers::{self, ArrayBufferBudget};
use super::fetch::{FetchJob, FetchResult, FetchWiring, StorageCtx};
use super::fetch_ops::{deliver_fetch_results, op_get_headers, op_signed_fetch};
use super::handle::{
    Command, SharedState, Watchdog, ensure_v8_initialized, finish, guard_oom, near_heap_limit_cb,
    watchdog_loop,
};

struct ClientChannel {
    /// The entity range this client was given when it connected. Frozen here so
    /// that neither the authority check nor the reclaim ever re-derives it from
    /// a config `registerScene` can move mid-session.
    start: u32,
    size: u32,

    /// Frames not yet read, and whether each still counts toward the scene
    /// total (frames held before the observer registers do not).
    inbound: VecDeque<(Vec<u8>, bool)>,
    inbound_bytes: usize,
    inbound_shared_bytes: usize,

    outbound: VecDeque<Vec<u8>>,
    outbound_bytes: usize,

    open_delivered: bool,

    closing: bool,
    closed: bool,
}

impl ClientChannel {
    fn new(start: u32, size: u32) -> Self {
        Self {
            start,
            size,
            inbound: VecDeque::new(),
            inbound_bytes: 0,
            inbound_shared_bytes: 0,
            outbound: VecDeque::new(),
            outbound_bytes: 0,
            open_delivered: false,
            closing: false,
            closed: false,
        }
    }

    /// Hands what is still queued back to the inbound budget.
    fn release_inbound(&mut self, budget: &InboundBudget, index: u32) {
        budget.release_parts(
            index,
            self.inbound_bytes,
            self.inbound.len(),
            self.inbound_shared_bytes,
        );
        self.inbound.clear();
        self.inbound_bytes = 0;
        self.inbound_shared_bytes = 0;
    }
}

pub(super) struct HostState {
    engine: Arc<Mutex<CrdtEngine>>,

    snapshot: Arc<Mutex<Vec<u8>>>,

    config: Arc<Mutex<ServerTransportConfig>>,

    slots: Arc<ClientSlots>,

    /// True once the scene installed a callable observer. Reclaim does **not**
    /// consult this: a scene that passes a non-function (or never calls
    /// `registerScene`) still gets its departed clients' ranges freed.
    observer_registered: bool,

    clients: std::collections::BTreeMap<u32, ClientChannel>,

    realm_name: String,

    scene_hash: String,

    client_outbound_max: usize,

    client_outbound_max_bytes: usize,

    /// What a client may queue before the scene registers its observer.
    pre_register_bytes: usize,
    pre_register_frames: usize,

    crdt_max_bytes: usize,

    inbound_budget: Arc<InboundBudget>,

    array_buffers: Arc<ArrayBufferBudget>,

    pub(super) storage: Option<StorageCtx>,

    pub(super) fetch_tx: Option<mpsc::UnboundedSender<FetchJob>>,

    pub(super) fetch_results: Option<std::sync::mpsc::Receiver<FetchResult>>,

    pub(super) pending_fetches: HashMap<u64, v8::Global<v8::PromiseResolver>>,

    pub(super) next_fetch_id: u64,

    pub(super) fetch_in_flight_max: usize,

    pub(super) fetch_max_body_bytes: usize,
}

type SharedHandles = (
    Arc<Mutex<CrdtEngine>>,
    Arc<Mutex<Vec<u8>>>,
    Arc<Mutex<ServerTransportConfig>>,
);

impl HostState {
    pub(super) fn with<R>(
        isolate: &mut v8::Isolate,
        f: impl FnOnce(&RefCell<HostState>) -> R,
    ) -> R {
        let ptr = isolate.get_data(0) as *const RefCell<HostState>;
        let cell = unsafe { &*ptr };
        f(cell)
    }

    fn shared_handles(isolate: &mut v8::Isolate) -> SharedHandles {
        Self::with(isolate, |c| {
            let h = c.borrow();
            (
                Arc::clone(&h.engine),
                Arc::clone(&h.snapshot),
                Arc::clone(&h.config),
            )
        })
    }
}

pub(super) struct SceneScript {
    pub scene_hash: String,
    pub source: String,
    pub realm_name: String,
}

pub(super) fn run_scene_thread(
    script: SceneScript,
    shared: Arc<SharedState>,
    watchdog: Arc<Watchdog>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    limits: RuntimeLimits,
    fetch: Option<FetchWiring>,
) {
    let SceneScript {
        scene_hash,
        source,
        realm_name,
    } = script;
    ensure_v8_initialized();

    let heap_max = limits.js_heap_limit_mb.saturating_mul(1024 * 1024);
    let array_buffers =
        ArrayBufferBudget::new(limits.js_array_buffer_limit_mb.saturating_mul(1024 * 1024));
    let isolate = &mut v8::Isolate::new(
        v8::CreateParams::default()
            .heap_limits(0, heap_max)
            .array_buffer_allocator(array_buffers::allocator(&array_buffers)),
    );

    let (storage, fetch_tx, fetch_results) = match fetch {
        Some(w) => (Some(w.ctx), Some(w.tx), Some(w.results)),
        None => (None, None, None),
    };
    let host: Box<RefCell<HostState>> = Box::new(RefCell::new(HostState {
        engine: Arc::clone(&shared.engine),
        snapshot: Arc::clone(&shared.snapshot),
        config: Arc::clone(&shared.config),
        slots: Arc::clone(&shared.slots),
        observer_registered: false,
        clients: std::collections::BTreeMap::new(),
        realm_name,
        scene_hash: scene_hash.clone(),
        client_outbound_max: limits.client_outbound_max,
        client_outbound_max_bytes: limits.client_outbound_max_bytes,
        pre_register_bytes: limits.client_inbound_max_bytes / 32,
        pre_register_frames: (limits.client_inbound_max / 32).max(1),
        crdt_max_bytes: limits.crdt_max_bytes,
        inbound_budget: Arc::clone(&shared.inbound),
        array_buffers,
        storage,
        fetch_tx,
        fetch_results,
        pending_fetches: HashMap::new(),
        next_fetch_id: 0,
        fetch_in_flight_max: limits.fetch_max_in_flight,
        fetch_max_body_bytes: limits.fetch_max_body_bytes,
    }));
    let host_ptr = host.as_ref() as *const RefCell<HostState> as *mut c_void;
    isolate.set_data(0, host_ptr);

    *watchdog.isolate.lock() = Some(isolate.thread_safe_handle());
    guard_oom(isolate, &shared, &watchdog, &scene_hash);
    let wd_ptr = Arc::as_ptr(&watchdog) as *mut c_void;
    isolate.add_near_heap_limit_callback(near_heap_limit_cb, wd_ptr);

    let wd_thread = {
        let watchdog = Arc::clone(&watchdog);
        std::thread::Builder::new()
            .name(format!("scene-wd-{scene_hash}"))
            .spawn(move || watchdog_loop(watchdog))
            .ok()
    };

    let budget_ms = limits.js_tick_budget_ms.max(1);

    v8::scope!(let handle_scope, isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    install_globals(scope, context);
    if let Err(e) = run_script(scope, array_buffers::HARDEN_JS) {
        tracing::error!(scene = %scene_hash, error = %e, "global hardening failed; not running the scene");
        finish(&shared, &watchdog, wd_thread, &scene_hash);
        return;
    }

    watchdog.arm(budget_ms);
    let eval = eval_scene(scope, &source);
    watchdog.disarm();
    if let Err(e) = eval {
        tracing::error!(scene = %scene_hash, error = %e, "scene eval failed");
        host.borrow_mut().pending_fetches.clear();
        finish(&shared, &watchdog, wd_thread, &scene_hash);
        return;
    }
    if watchdog.was_terminated() {
        tracing::error!(scene = %scene_hash, "scene eval exceeded wall-clock budget; aborting scene");
        host.borrow_mut().pending_fetches.clear();
        finish(&shared, &watchdog, wd_thread, &scene_hash);
        return;
    }

    watchdog.arm(budget_ms);
    let r = call_export(scope, context, "onStart", None);
    watchdog.disarm();
    if let Err(e) = r {
        tracing::warn!(scene = %scene_hash, error = %e, "onStart threw");
    }
    if watchdog.was_terminated() {
        tracing::error!(scene = %scene_hash, "onStart exceeded wall-clock budget; aborting scene");
        host.borrow_mut().pending_fetches.clear();
        finish(&shared, &watchdog, wd_thread, &scene_hash);
        return;
    }
    watchdog.arm(budget_ms);
    drain_set_immediate(scope, context);
    watchdog.disarm();
    if watchdog.was_terminated() {
        tracing::error!(scene = %scene_hash, "setImmediate exceeded wall-clock budget; aborting scene");
        host.borrow_mut().pending_fetches.clear();
        finish(&shared, &watchdog, wd_thread, &scene_hash);
        return;
    }

    let has_on_update = export_is_function(scope, context, "onUpdate");
    if !has_on_update {
        tracing::warn!(
            scene = %scene_hash,
            "scene exports no onUpdate; running as static scene (no game loop)"
        );
    }

    if has_on_update {
        watchdog.arm(budget_ms);
        let _ = call_export(scope, context, "onUpdate", Some(0.0));
        watchdog.disarm();
        if watchdog.was_terminated() {
            tracing::error!(scene = %scene_hash, "onUpdate exceeded wall-clock budget; aborting scene");
            host.borrow_mut().pending_fetches.clear();
            finish(&shared, &watchdog, wd_thread, &scene_hash);
            return;
        }
        watchdog.arm(budget_ms);
        drain_set_immediate(scope, context);
        watchdog.disarm();
        if watchdog.was_terminated() {
            tracing::error!(scene = %scene_hash, "setImmediate exceeded wall-clock budget; aborting scene");
            host.borrow_mut().pending_fetches.clear();
            finish(&shared, &watchdog, wd_thread, &scene_hash);
            return;
        }
    }

    let tick = std::time::Duration::from_micros(1_000_000 / 30);
    let mut last = std::time::Instant::now();
    let update_failure_cap = limits.js_update_failure_cap.max(1);
    let mut update_failures = 0usize;

    loop {
        let mut shutdown = !shared.running.load(std::sync::atomic::Ordering::SeqCst);
        while !shutdown {
            match rx.try_recv() {
                Ok(Command::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Ok(Command::ClientOpen { index, start, size }) => {
                    let mut h = host.borrow_mut();
                    let budget = Arc::clone(&h.inbound_budget);
                    match h.clients.get_mut(&index) {
                        Some(existing) if existing.closed => {
                            existing.release_inbound(&budget, index);
                            *existing = ClientChannel::new(start, size);
                        }
                        Some(_) => {}
                        None => {
                            h.clients.insert(index, ClientChannel::new(start, size));
                        }
                    }
                }
                Ok(Command::ClientCrdt { index, body }) => {
                    let mut h = host.borrow_mut();
                    let floor = h.config.lock().network_floor();
                    let budget = Arc::clone(&h.inbound_budget);
                    let reserved = body.len();

                    let Some((start, size)) = h
                        .clients
                        .get(&index)
                        .filter(|c| !c.closing && !c.closed)
                        .map(|c| (c.start, c.size))
                    else {
                        budget.release(index, reserved, 1);
                        tracing::warn!(
                            scene = %scene_hash, index,
                            "crdt from a client with no open channel; dropped"
                        );
                        continue;
                    };
                    // Before the observer registers, keep only the newest few frames.
                    let early = (!h.observer_registered)
                        .then_some((h.pre_register_bytes, h.pre_register_frames));
                    let foreign: Vec<(u32, u32)> = h
                        .clients
                        .iter()
                        .filter(|(other, c)| **other != index && !c.closed)
                        .map(|(_, c)| (c.start, c.size))
                        .collect();

                    if let Some(c) = h.clients.get_mut(&index) {
                        let guard = AuthorityGuard {
                            authority: Authority::Client { start, size },
                            floor,
                            foreign_ranges: &foreign,
                        };
                        let msgs = guard.filter(decode_client_batch(&body, start, size));
                        if msgs.is_empty() {
                            budget.release(index, reserved, 1);
                        } else {
                            let kept = encode_batch(&msgs);
                            budget.release(index, reserved.saturating_sub(kept.len()), 0);
                            // Unread early frames go to the held share, not the readable budget.
                            let shared = early.is_none();
                            if shared {
                                c.inbound_shared_bytes += kept.len();
                                c.inbound_bytes += kept.len();
                                c.inbound.push_back((kept, true));
                            } else {
                                // The newest early frame wins: make room from this
                                // client's own oldest held frames first.
                                let (max_bytes, max_frames) = early.unwrap_or_default();
                                loop {
                                    let fits = c.inbound_bytes + kept.len() <= max_bytes
                                        && c.inbound.len() < max_frames;
                                    if fits && budget.hold(kept.len()) {
                                        c.inbound_bytes += kept.len();
                                        c.inbound.push_back((kept, false));
                                        break;
                                    }
                                    let Some((old, was_shared)) = c.inbound.pop_front() else {
                                        budget.release(index, kept.len(), 1);
                                        break;
                                    };
                                    let shared_bytes = if was_shared { old.len() } else { 0 };
                                    c.inbound_bytes -= old.len();
                                    c.inbound_shared_bytes -= shared_bytes;
                                    budget.release_parts(index, old.len(), 1, shared_bytes);
                                }
                            }
                        }
                        if let Some((max_bytes, max_frames)) = early {
                            while c.inbound_bytes > max_bytes || c.inbound.len() > max_frames {
                                let Some((old, shared)) = c.inbound.pop_front() else {
                                    break;
                                };
                                let shared_bytes = if shared { old.len() } else { 0 };
                                c.inbound_bytes -= old.len();
                                c.inbound_shared_bytes -= shared_bytes;
                                budget.release_parts(index, old.len(), 1, shared_bytes);
                            }
                        }
                    }
                }
                Ok(Command::ClientClose { index }) => {
                    if let Some(c) = host.borrow_mut().clients.get_mut(&index) {
                        c.closing = true;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    shutdown = true;
                    break;
                }
            }
        }
        if shutdown {
            break;
        }

        watchdog.arm(budget_ms);
        deliver_client_events(scope, context);
        watchdog.disarm();
        if watchdog.was_terminated() {
            tracing::error!(scene = %scene_hash, "observer callback exceeded budget / OOM; aborting scene");
            break;
        }

        watchdog.arm(budget_ms);
        deliver_fetch_results(scope);
        watchdog.disarm();
        if watchdog.was_terminated() {
            tracing::error!(scene = %scene_hash, "fetch continuation exceeded budget / OOM; aborting scene");
            break;
        }

        if has_on_update {
            let now = std::time::Instant::now();
            let dt = (now - last).as_secs_f64();
            last = now;
            watchdog.arm(budget_ms);
            let r = call_export(scope, context, "onUpdate", Some(dt));
            watchdog.disarm();
            if watchdog.was_terminated() {
                tracing::error!(scene = %scene_hash, "onUpdate exceeded wall-clock budget / OOM; aborting scene");
                break;
            }
            if let Err(e) = r {
                update_failures += 1;
                if update_failures >= update_failure_cap {
                    tracing::error!(
                        scene = %scene_hash, error = %e, failures = update_failures,
                        "onUpdate threw on too many consecutive frames; stopping scene"
                    );
                    break;
                }
                tracing::warn!(
                    scene = %scene_hash, error = %e, failures = update_failures,
                    "onUpdate threw; skipping frame"
                );
            } else {
                update_failures = 0;
                watchdog.arm(budget_ms);
                drain_set_immediate(scope, context);
                watchdog.disarm();
                if watchdog.was_terminated() {
                    tracing::error!(scene = %scene_hash, "setImmediate exceeded wall-clock budget / OOM; aborting scene");
                    break;
                }
            }
        }

        flush_outbound(&host, &shared);

        {
            let mut h = host.borrow_mut();
            let budget = Arc::clone(&h.inbound_budget);
            h.clients.retain(|index, c| {
                if c.closed {
                    c.release_inbound(&budget, *index);
                }
                !c.closed
            });
        }

        if !has_on_update {
            std::thread::sleep(std::time::Duration::from_millis(50));
        } else {
            let elapsed = std::time::Instant::now() - last;
            if elapsed < tick {
                std::thread::sleep(tick - elapsed);
            }
        }
    }

    host.borrow_mut().pending_fetches.clear();

    finish(&shared, &watchdog, wd_thread, &scene_hash);
}

/// Hands queued frames to each socket while the bytes it has not written
/// stay within the client's outbound budget; the rest waits for a later tick.
fn flush_outbound(host: &RefCell<HostState>, shared: &SharedState) {
    let mut host = host.borrow_mut();
    let max_bytes = host.client_outbound_max_bytes;
    for (index, ch) in host.clients.iter_mut() {
        // A closed client's slot may already belong to a new connection.
        if ch.closed {
            ch.outbound.clear();
            ch.outbound_bytes = 0;
            continue;
        }
        if ch.outbound.is_empty() {
            continue;
        }
        let Some(sender) = shared.outbound.get(index) else {
            ch.outbound.clear();
            ch.outbound_bytes = 0;
            continue;
        };
        let meter = shared.outbound_meters.get(index).map(|m| Arc::clone(&m));
        while let Some(body) = ch.outbound.front() {
            let frame = crate::scene_state::runtime::frame_crdt(body);
            if let Some(m) = &meter {
                let in_flight = m.load(std::sync::atomic::Ordering::SeqCst);
                if in_flight > 0 && in_flight.saturating_add(frame.len()) > max_bytes {
                    break;
                }
                m.fetch_add(frame.len(), std::sync::atomic::Ordering::SeqCst);
            }
            let len = frame.len();
            match sender.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if let Some(m) = &meter {
                        m.fetch_sub(len, std::sync::atomic::Ordering::SeqCst);
                    }
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if let Some(m) = &meter {
                        m.fetch_sub(len, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
            if let Some(sent) = ch.outbound.pop_front() {
                ch.outbound_bytes = ch.outbound_bytes.saturating_sub(sent.len());
            }
        }
    }
}

/// A copy of a scene buffer; over `max` bytes it throws a `RangeError` instead.
fn read_uint8array(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
    max: usize,
) -> Result<Option<Vec<u8>>, ()> {
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(val) else {
        return Ok(None);
    };
    let len = view.byte_length();
    if len > max {
        throw_range_error(scope, &format!("buffer of {len} bytes exceeds {max}"));
        return Err(());
    }
    let mut out = vec![0u8; len];
    view.copy_contents(&mut out);
    Ok(Some(out))
}

fn throw_range_error(scope: &mut v8::PinScope, message: &str) {
    let msg = str(scope, message);
    let exc = v8::Exception::range_error(scope, msg);
    scope.throw_exception(exc);
}

/// A `Uint8Array` copy of `bytes` charged to the isolate's ArrayBuffer
/// budget; over budget it throws a `RangeError` and returns `None`.
fn make_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Option<v8::Local<'s, v8::Uint8Array>> {
    let budget = HostState::with(scope, |c| Arc::clone(&c.borrow().array_buffers));
    if let Some(made) = array_buffers::uint8array(scope, &budget, bytes) {
        return Some(made);
    }
    // V8 collects garbage before failing its own allocations; do the same.
    scope.low_memory_notification();
    let made = array_buffers::uint8array(scope, &budget, bytes);
    if made.is_none() {
        throw_range_error(scope, "Array buffer allocation failed");
    }
    made
}

fn run_script(scope: &mut v8::PinScope, source: &str) -> Result<(), String> {
    v8::tc_scope!(let tc, scope);
    let code = v8::String::new(tc, source).ok_or("source too large")?;
    let script = v8::Script::compile(tc, code, None).ok_or_else(|| caught(tc))?;
    if script.run(tc).is_none() {
        return Err(caught(tc));
    }
    Ok(())
}

fn crdt_max_bytes(scope: &mut v8::PinScope) -> usize {
    HostState::with(scope, |c| c.borrow().crdt_max_bytes)
}

pub(super) fn str<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, s).unwrap_or_else(|| v8::String::empty(scope))
}

pub(super) fn set_prop(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
    value: v8::Local<v8::Value>,
) {
    let k = str(scope, key).into();
    obj.set(scope, k, value);
}

fn set_fn(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let Some(f) = v8::Function::new(scope, cb) else {
        return;
    };
    let k = str(scope, name).into();
    obj.set(scope, k, f.into());
}

/// Everything the scene itself authors goes through here: the range check is
/// on the **apply**, not on a queue somewhere upstream of it.
///
/// The scene is the authority for its own world, so it may write any network
/// entity -- including relaying another client's. What it may not do is author
/// the renderer-local block (`ServerTransportConfig::network_floor`), which is
/// a *different* entity on every client.
fn apply_as_server(scope: &mut v8::PinScope, bytes: &[u8]) -> ServerApply {
    let (engine, snapshot, cfg) = HostState::shared_handles(scope);
    let floor = cfg.lock().network_floor();

    let decoded = decode_batch(bytes);
    let total = decoded.len();
    let guard = AuthorityGuard {
        authority: Authority::Server,
        floor,
        foreign_ranges: &[],
    };
    let msgs = guard.filter(decoded);
    let dropped = total - msgs.len();
    if dropped > 0 {
        let scene_hash = HostState::with(scope, |c| c.borrow().scene_hash.clone());
        tracing::warn!(
            scene = %scene_hash,
            dropped,
            floor,
            "scene tried to author renderer-local entities; dropped"
        );
    }

    let mut eng = engine.lock();
    if !msgs.is_empty() {
        eng.apply_batch(&msgs);
    }
    *snapshot.lock() = eng.snapshot();
    ServerApply {
        admitted: msgs,
        dropped,
    }
}

struct ServerApply {
    admitted: Vec<crate::scene_state::crdt::CrdtMessage>,
    dropped: usize,
}

fn op_crdt_send_to_renderer(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let mut payload: Option<Vec<u8>> = None;
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(args.get(0)) {
        let key = str(scope, "data").into();
        if let Some(data_val) = obj.get(scope, key) {
            let max = crdt_max_bytes(scope);
            let Ok(read) = read_uint8array(scope, data_val, max) else {
                return;
            };
            payload = read;
        }
    }
    let _ = apply_as_server(scope, payload.as_deref().unwrap_or(&[]));

    let result = v8::Object::new(scope);
    let empty = v8::Array::new(scope, 0);
    set_prop(scope, result, "data", empty.into());
    rv.set(result.into());
}

fn op_crdt_get_state(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let (_engine, snapshot, _cfg) = HostState::shared_handles(scope);
    let snap = snapshot.lock().clone();
    let result = v8::Object::new(scope);
    let has = !snap.is_empty();
    let has_v = v8::Boolean::new(scope, has);
    set_prop(scope, result, "hasEntities", has_v.into());
    let mut items: Vec<v8::Local<v8::Value>> = Vec::new();
    if has {
        let Some(ua) = make_uint8array(scope, &snap) else {
            return;
        };
        items.push(ua.into());
    }
    // Built whole: element setters on Array.prototype never run.
    let arr = v8::Array::new_with_elements(scope, &items);
    set_prop(scope, result, "data", arr.into());
    rv.set(result.into());
}

fn op_is_server(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let result = v8::Object::new(scope);
    let t = v8::Boolean::new(scope, true);
    set_prop(scope, result, "isServer", t.into());
    rv.set(result.into());
}

fn op_send_batch(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let result = v8::Object::new(scope);
    let arr = v8::Array::new(scope, 0);
    set_prop(scope, result, "events", arr.into());
    rv.set(result.into());
}

fn op_get_realm(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let realm_name = HostState::with(scope, |c| c.borrow().realm_name.clone());
    let info = v8::Object::new(scope);
    let f = v8::Boolean::new(scope, false);
    set_prop(scope, info, "isPreview", f.into());
    let name = str(scope, &realm_name).into();
    set_prop(scope, info, "realmName", name);
    let result = v8::Object::new(scope);
    set_prop(scope, result, "realmInfo", info.into());
    rv.set(result.into());
}

fn op_get_scene_information(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let hash = HostState::with(scope, |c| c.borrow().scene_hash.clone());
    let result = v8::Object::new(scope);
    let urn = str(scope, &hash).into();
    set_prop(scope, result, "urn", urn);
    let base = str(scope, "").into();
    set_prop(scope, result, "baseUrl", base);
    let content = v8::Array::new(scope, 0);
    set_prop(scope, result, "content", content.into());
    let meta = str(scope, "{}").into();
    set_prop(scope, result, "metadataJson", meta);
    rv.set(result.into());
}

fn op_read_file(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let result = v8::Object::new(scope);
    let Some(ua) = make_uint8array(scope, &[]) else {
        return;
    };
    set_prop(scope, result, "content", ua.into());
    let h = str(scope, "").into();
    set_prop(scope, result, "hash", h);
    rv.set(result.into());
}

fn op_empty_promise(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        return;
    };
    let promise = resolver.get_promise(scope);
    let result = v8::Object::new(scope);
    resolver.resolve(scope, result.into());
    rv.set(promise.into());
}

fn op_get_user_data(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        return;
    };
    let promise = resolver.get_promise(scope);
    let address = HostState::with(scope, |c| {
        c.borrow()
            .storage
            .as_ref()
            .and_then(|s| s.delegation.lock().clone())
            .map(|d| d.ephemeral_address)
    });
    let result = v8::Object::new(scope);
    if let Some(address) = address {
        let data = v8::Object::new(scope);
        let uid = str(scope, &address).into();
        set_prop(scope, data, "userId", uid);
        let web3 = v8::Boolean::new(scope, false);
        set_prop(scope, data, "hasConnectedWeb3", web3.into());
        set_prop(scope, result, "data", data.into());
    }
    resolver.resolve(scope, result.into());
    rv.set(promise.into());
}

fn op_register_scene(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if let Ok(cfg) = v8::Local::<v8::Object>::try_from(args.get(0)) {
        let reserved = get_u32_prop(scope, cfg, "reservedLocalEntities").unwrap_or(512);
        let (server_limit, client_limit) = {
            let key = str(scope, "networkEntitiesLimit").into();
            if let Some(nv) = cfg.get(scope, key) {
                if let Ok(no) = v8::Local::<v8::Object>::try_from(nv) {
                    (
                        get_u32_prop(scope, no, "serverLimit").unwrap_or(512),
                        get_u32_prop(scope, no, "clientLimit").unwrap_or(512),
                    )
                } else {
                    (512, 512)
                }
            } else {
                (512, 512)
            }
        };
        HostState::with(scope, |c| {
            let h = c.borrow();
            let live = h.clients.values().filter(|ch| !ch.closed).count();
            if live > 0 {
                let current = *h.config.lock();
                if current.reserved_local_entities != reserved
                    || current.server_network_entities_limit != server_limit
                    || current.client_network_entities_limit != client_limit
                {
                    tracing::warn!(
                        scene = %h.scene_hash,
                        live,
                        "registerScene changed the entity-range config while clients were \
                         connected; the config is frozen and the change was ignored"
                    );
                }
            } else {
                *h.config.lock() = ServerTransportConfig {
                    reserved_local_entities: reserved,
                    server_network_entities_limit: server_limit,
                    client_network_entities_limit: client_limit,
                };
            }
        });
    }

    let observer = args.get(1);
    if observer.is_function() {
        let ctx = scope.get_current_context();
        let global = ctx.global(scope);
        let key = str(scope, "__observer").into();
        global.set(scope, key, observer);
        HostState::with(scope, |c| c.borrow_mut().observer_registered = true);
    } else if !observer.is_undefined() {
        let scene_hash = HostState::with(scope, |c| c.borrow().scene_hash.clone());
        tracing::warn!(
            scene = %scene_hash,
            "registerScene's second argument is not a function; no client observer installed"
        );
    }
}

fn op_update_crdt_state(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let max = crdt_max_bytes(scope);
    if let Ok(Some(bytes)) = read_uint8array(scope, args.get(0), max) {
        let (_engine, snapshot, _cfg) = HostState::shared_handles(scope);
        *snapshot.lock() = bytes;
    }
}

fn op_console_log(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let mut parts = Vec::new();
    for i in 0..args.length() {
        let s = args
            .get(i)
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
        parts.push(s);
    }
    tracing::info!(target: "scene_console", "{}", parts.join(" "));
}

fn op_set_immediate(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let fn_arg = args.get(0);
    if !fn_arg.is_function() {
        return;
    }
    let ctx = scope.get_current_context();
    let global = ctx.global(scope);
    let key = str(scope, "__setImmediate").into();
    let list = match global.get(scope, key) {
        Some(v) if v.is_array() => v8::Local::<v8::Array>::try_from(v).unwrap(),
        _ => {
            let arr = v8::Array::new(scope, 0);
            global.set(scope, key, arr.into());
            arr
        }
    };
    let len = list.length();
    list.set_index(scope, len, fn_arg);
}

fn op_require(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let name = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let module = name.strip_prefix("~system/").unwrap_or(&name);
    let obj = build_system_module(scope, module);
    match obj {
        Some(o) => rv.set(o.into()),
        None => {
            let msg = str(scope, &format!("Unknown module {name}"));
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
        }
    }
}

fn op_restricted(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let msg = str(scope, "Disabled on server");
    let exc = v8::Exception::error(scope, msg);
    scope.throw_exception(exc);
}

fn get_u32_prop(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, key: &str) -> Option<u32> {
    let k = str(scope, key).into();
    let v = obj.get(scope, k)?;
    v.uint32_value(scope)
}

fn build_system_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    module: &str,
) -> Option<v8::Local<'s, v8::Object>> {
    let obj = v8::Object::new(scope);
    match module {
        "EngineApi" => {
            set_fn(scope, obj, "crdtSendToRenderer", op_crdt_send_to_renderer);
            set_fn(scope, obj, "crdtGetState", op_crdt_get_state);
            set_fn(scope, obj, "isServer", op_is_server);
            set_fn(scope, obj, "sendBatch", op_send_batch);
        }
        "Runtime" => {
            set_fn(scope, obj, "getRealm", op_get_realm);
            set_fn(scope, obj, "getSceneInformation", op_get_scene_information);
            set_fn(scope, obj, "readFile", op_read_file);
        }
        "UserIdentity" => {
            set_fn(scope, obj, "getUserData", op_get_user_data);
        }
        "SignedFetch" => {
            set_fn(scope, obj, "signedFetch", op_signed_fetch);
            set_fn(scope, obj, "getHeaders", op_get_headers);
        }

        "RestrictedActions" => {
            for m in [
                "movePlayerTo",
                "teleportTo",
                "triggerEmote",
                "triggerSceneEmote",
                "stopEmote",
                "openExternalUrl",
                "openNftDialog",
                "changeRealm",
            ] {
                set_fn(scope, obj, m, op_empty_promise);
            }
        }
        "EnvironmentApi" | "EnvironmentAPI" => {}

        _ => {}
    }
    Some(obj)
}

fn install_globals(scope: &mut v8::PinScope, context: v8::Local<v8::Context>) {
    let global = context.global(scope);

    let module = v8::Object::new(scope);
    let exports = v8::Object::new(scope);
    set_prop(scope, module, "exports", exports.into());
    set_prop(scope, global, "module", module.into());
    set_prop(scope, global, "exports", exports.into());

    let console = v8::Object::new(scope);
    for m in ["log", "info", "debug", "trace", "warning", "error"] {
        set_fn(scope, console, m, op_console_log);
    }
    set_prop(scope, global, "console", console.into());

    set_fn(scope, global, "require", op_require);
    set_fn(scope, global, "setImmediate", op_set_immediate);
    set_fn(scope, global, "registerScene", op_register_scene);
    set_fn(scope, global, "updateCRDTState", op_update_crdt_state);

    set_fn(scope, global, "fetch", op_restricted);
    set_fn(scope, global, "WebSocket", op_restricted);

    let g: v8::Local<v8::Value> = global.into();
    set_prop(scope, global, "self", g);
    set_prop(scope, global, "global", g);
}

fn eval_scene(scope: &mut v8::PinScope, source: &str) -> Result<(), String> {
    v8::tc_scope!(let tc, scope);
    let wrapped = format!(
        ";(function (module, exports) {{ {source}\n}}).call(module.exports, module, module.exports);"
    );
    let code = v8::String::new(tc, &wrapped).ok_or("source too large")?;
    let script = v8::Script::compile(tc, code, None).ok_or_else(|| caught(tc))?;
    if script.run(tc).is_none() {
        return Err(caught(tc));
    }
    Ok(())
}

fn caught(tc: &mut v8::PinnedRef<v8::TryCatch<v8::HandleScope>>) -> String {
    tc.exception()
        .and_then(|e| e.to_string(tc))
        .map(|s| s.to_rust_string_lossy(tc))
        .unwrap_or_else(|| "unknown JS error".into())
}

fn export_is_function(
    scope: &mut v8::PinScope,
    context: v8::Local<v8::Context>,
    name: &str,
) -> bool {
    let global = context.global(scope);
    let exports_key = str(scope, "exports").into();
    let Some(exports) = global.get(scope, exports_key) else {
        return false;
    };
    let Ok(exports) = v8::Local::<v8::Object>::try_from(exports) else {
        return false;
    };
    let fk = str(scope, name).into();
    matches!(exports.get(scope, fk), Some(v) if v.is_function())
}

fn call_export(
    scope: &mut v8::PinScope,
    context: v8::Local<v8::Context>,
    name: &str,
    dt: Option<f64>,
) -> Result<(), String> {
    let global = context.global(scope);
    let exports_key = str(scope, "exports").into();
    let exports = global.get(scope, exports_key).ok_or("no exports")?;
    let exports = v8::Local::<v8::Object>::try_from(exports).map_err(|_| "exports not object")?;
    let fk = str(scope, name).into();
    let f = match exports.get(scope, fk) {
        Some(v) if v.is_function() => v8::Local::<v8::Function>::try_from(v).unwrap(),
        _ => return Ok(()),
    };

    v8::tc_scope!(let tc, scope);
    let recv: v8::Local<v8::Value> = exports.into();
    let result = if let Some(dt) = dt {
        let arg = v8::Number::new(tc, dt).into();
        f.call(tc, recv, &[arg])
    } else {
        f.call(tc, recv, &[])
    };
    if result.is_none() {
        return Err(caught(tc));
    }

    tc.perform_microtask_checkpoint();
    Ok(())
}

fn drain_set_immediate(scope: &mut v8::PinScope, context: v8::Local<v8::Context>) {
    let global = context.global(scope);
    let key = str(scope, "__setImmediate").into();
    let list = match global.get(scope, key) {
        Some(v) if v.is_array() => v8::Local::<v8::Array>::try_from(v).unwrap(),
        _ => return,
    };
    let len = list.length();
    if len == 0 {
        return;
    }
    let fresh = v8::Array::new(scope, 0);
    global.set(scope, key, fresh.into());

    let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
    for i in 0..len {
        if let Some(item) = list.get_index(scope, i)
            && item.is_function()
        {
            let f = v8::Local::<v8::Function>::try_from(item).unwrap();
            v8::tc_scope!(let tc, scope);
            let _ = f.call(tc, recv, &[]);
        }
    }
}

/// Fire the scene's client observer (if it installed a callable one) and -- in
/// all cases -- reclaim the range of every client that has gone away.
///
/// Reclaim used to sit behind two guards: `observer_registered`, and the
/// `__observer` lookup below. `registerScene(cfg, 42)` set the former without
/// setting the latter, and a scene that never called `registerScene` set
/// neither, so departed clients were never reclaimed and their entities leaked
/// for the lifetime of the scene. Reclaim is a server invariant, not a
/// courtesy the scene opts into: it runs regardless.
fn deliver_client_events(scope: &mut v8::PinScope, context: v8::Local<v8::Context>) {
    let (to_open, to_close) = HostState::with(scope, |c| {
        let h = c.borrow();
        let mut to_open: Vec<u32> = Vec::new();
        let mut to_close: Vec<(u32, u32, u32)> = Vec::new();
        for (index, ch) in h.clients.iter() {
            if !ch.open_delivered && !ch.closing && !ch.closed {
                to_open.push(*index);
            }
            if ch.closing && !ch.closed {
                to_close.push((*index, ch.start, ch.size));
            }
        }
        (to_open, to_close)
    });
    if to_open.is_empty() && to_close.is_empty() {
        return;
    }

    let global = context.global(scope);
    let obs_key = str(scope, "__observer").into();
    let observer = match global.get(scope, obs_key) {
        Some(v) if v.is_function() => Some(v8::Local::<v8::Function>::try_from(v).unwrap()),
        _ => None,
    };
    let recv: v8::Local<v8::Value> = v8::undefined(scope).into();

    if let Some(observer) = observer {
        for index in to_open {
            HostState::with(scope, |c| {
                if let Some(ch) = c.borrow_mut().clients.get_mut(&index) {
                    ch.open_delivered = true;
                }
            });
            let client_obj = build_client_object(scope, index);
            let event = v8::Object::new(scope);
            let ty = str(scope, "open").into();
            set_prop(scope, event, "type", ty);
            let id = str(scope, &index.to_string()).into();
            set_prop(scope, event, "clientId", id);
            set_prop(scope, event, "client", client_obj.into());

            v8::tc_scope!(let tc, scope);
            let _ = observer.call(tc, recv, &[event.into()]);
        }
    }

    for (index, start, size) in to_close {
        if let Some(observer) = observer {
            let event = v8::Object::new(scope);
            let ty = str(scope, "close").into();
            set_prop(scope, event, "type", ty);
            let id = str(scope, &index.to_string()).into();
            set_prop(scope, event, "clientId", id);
            v8::tc_scope!(let tc, scope);
            let _ = observer.call(tc, recv, &[event.into()]);
        }

        let (engine, snapshot, _config) = HostState::shared_handles(scope);
        let deletes = engine.lock().reclaim_range(start, size);
        if !deletes.is_empty() {
            let body = crate::scene_state::crdt::encode_batch(&deletes);
            *snapshot.lock() = engine.lock().snapshot();
            HostState::with(scope, |c| {
                let mut h = c.borrow_mut();
                for (other, ch) in h.clients.iter_mut() {
                    if *other != index && !ch.closed {
                        ch.outbound_bytes += body.len();
                        ch.outbound.push_back(body.clone());
                    }
                }
            });
        }
        HostState::with(scope, |c| {
            let mut h = c.borrow_mut();
            let budget = Arc::clone(&h.inbound_budget);
            if let Some(ch) = h.clients.get_mut(&index) {
                ch.release_inbound(&budget, index);
                ch.closed = true;
            }
            h.slots.release(index);
        });
    }
}

fn build_client_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    index: u32,
) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);

    let idx_key = str(scope, "__index").into();
    let idx_val = v8::Integer::new_from_unsigned(scope, index).into();
    obj.set(scope, idx_key, idx_val);
    set_fn(scope, obj, "sendCrdtMessage", op_client_send);
    set_fn(scope, obj, "getMessages", op_client_get_messages);
    obj
}

fn this_index(scope: &mut v8::PinScope, args: &v8::FunctionCallbackArguments) -> Option<u32> {
    let this = args.this();
    let key = str(scope, "__index").into();
    this.get(scope, key)?.uint32_value(scope)
}

fn op_client_send(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(index) = this_index(scope, &args) else {
        return;
    };
    let max = HostState::with(scope, |c| c.borrow().client_outbound_max_bytes);
    let Ok(Some(bytes)) = read_uint8array(scope, args.get(0), max) else {
        return;
    };
    if bytes.is_empty() {
        return;
    }

    let outcome = apply_as_server(scope, &bytes);

    let payload = if outcome.dropped == 0 {
        bytes
    } else {
        encode_batch(&outcome.admitted)
    };
    if payload.is_empty() {
        return;
    }

    HostState::with(scope, |c| {
        let mut h = c.borrow_mut();
        let cap = h.client_outbound_max;
        let max_bytes = h.client_outbound_max_bytes;
        if let Some(ch) = h.clients.get_mut(&index) {
            while ch.outbound.len() >= cap
                || ch.outbound_bytes.saturating_add(payload.len()) > max_bytes
            {
                let Some(dropped) = ch.outbound.pop_front() else {
                    break;
                };
                ch.outbound_bytes = ch.outbound_bytes.saturating_sub(dropped.len());
            }
            ch.outbound_bytes += payload.len();
            ch.outbound.push_back(payload);
        }
    });
}

fn op_client_get_messages(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(index) = this_index(scope, &args) else {
        let empty = v8::Array::new(scope, 0);
        rv.set(empty.into());
        return;
    };
    let msgs: Vec<Vec<u8>> = HostState::with(scope, |c| {
        c.borrow()
            .clients
            .get(&index)
            .map(|ch| ch.inbound.iter().map(|(m, _)| m.clone()).collect())
            .unwrap_or_default()
    });
    let mut items: Vec<v8::Local<v8::Value>> = Vec::with_capacity(msgs.len());
    for m in &msgs {
        // On failure the queue stays as it was for the next call.
        let Some(ua) = make_uint8array(scope, m) else {
            return;
        };
        items.push(ua.into());
    }
    // Built whole: no element setter can run scene code before the drain.
    let arr = v8::Array::new_with_elements(scope, &items);
    HostState::with(scope, |c| {
        let mut h = c.borrow_mut();
        let budget = Arc::clone(&h.inbound_budget);
        if let Some(ch) = h.clients.get_mut(&index) {
            let n = msgs.len().min(ch.inbound.len());
            let (mut taken, mut shared) = (0, 0);
            for (m, counted) in ch.inbound.drain(..n) {
                taken += m.len();
                if counted {
                    shared += m.len();
                }
            }
            ch.inbound_bytes = ch.inbound_bytes.saturating_sub(taken);
            ch.inbound_shared_bytes = ch.inbound_shared_bytes.saturating_sub(shared);
            budget.release_parts(index, taken, n, shared);
        }
    });
    rv.set(arr.into());
}

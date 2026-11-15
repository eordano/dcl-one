use std::time::Duration;

use bevy::{
    prelude::*,
    tasks::{IoTaskPool, Task},
};
use bevy_console::ConsoleCommand;
use common::{
    base_domain::{self, Service},
    client_login::{retry_delay_ms, retryable_status, DurableLogins, PendingClientLogin},
    util::{reqwest_client, TaskCompat, TaskExt},
};
use console::{AddDebugSection, DebugSection, DoAddConsoleCommand};
use wallet::Wallet;

pub(super) struct ClientLoginPlugin;

impl Plugin for ClientLoginPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ClientLoginDelivery>()
            .add_console_command::<ClientLoginDebug, _>(debug)
            .add_debug_section(DebugSection {
                title: "Client login events",
                query: "/client_login_state",
                fields: &[
                    ("Delivery active", "active"),
                    ("Pending events", "pending"),
                    ("Queue saved", "durable"),
                    ("Memory-only delivery", "memoryOnly"),
                    ("Storage failures", "storageFailures"),
                    ("Storage in flight", "storageInFlight"),
                    ("Request in flight", "inFlight"),
                    ("Attempts", "attempts"),
                    ("Result", "result"),
                    ("Discarded events", "discarded"),
                    ("Retry in milliseconds", "retryInMs"),
                ],
                actions: &[],
                inputs: &[],
            });
    }
}

#[derive(Resource)]
pub(super) struct ClientLoginDelivery {
    queue: DurableLogins,
    storage: Option<Task<Result<Stored, String>>>,
    storage_retry: u64,
    storage_failures: u32,
    memory_only: bool,
    storage_needs_load: bool,
    active: Option<String>,
    task: Option<Task<AttemptResult>>,
    event_id: Option<String>,
    attempts: u32,
    next_attempt: u64,
    result: &'static str,
    discarded: usize,
}

impl Default for ClientLoginDelivery {
    fn default() -> Self {
        Self {
            queue: Default::default(),
            storage: None,
            storage_retry: 0,
            storage_failures: 0,
            memory_only: false,
            storage_needs_load: true,
            active: None,
            task: None,
            event_id: None,
            attempts: 0,
            next_attempt: 0,
            result: "",
            discarded: 0,
        }
    }
}

enum Stored {
    Loaded(Vec<PendingClientLogin>),
    Saved(Vec<String>),
}

#[derive(Debug, PartialEq, Eq)]
enum AttemptResult {
    Accepted,
    Retry,
    EndpointUnavailable,
    Rejected,
}

fn now_ms() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn endpoint() -> &'static str {
    // Launch options latch service resolution before the delivery systems run.
    static ENDPOINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ENDPOINT.get_or_init(|| base_domain::url(Service::SocialApi, "/v1/client-login"))
}

impl ClientLoginDelivery {
    pub(super) fn pause(&mut self) {
        self.task = None;
        self.event_id = None;
        self.active = None;
        self.attempts = 0;
        self.next_attempt = 0;
        self.result = "paused";
    }

    pub(super) fn completed(&mut self, wallet: &Wallet) {
        self.pause();
        let Some(address) = wallet.address().filter(|_| !wallet.is_guest()) else {
            return;
        };
        let address = format!("{address:#x}");
        self.discarded += self.queue.push(PendingClientLogin {
            wallet: address.clone(),
            endpoint: endpoint().to_owned(),
            login_id: uuid::Uuid::new_v4().to_string(),
            occurred_at: now_ms(),
        });
        self.active = Some(address);
        self.result = "queued";
    }

    pub(super) fn drive(&mut self, wallet: &Wallet) {
        self.drive_at(wallet, now_ms(), endpoint());
    }

    fn storage_failed(&mut self, now: u64) {
        self.queue.finish_save(false);
        self.storage_failures = self.storage_failures.saturating_add(1);
        if self.storage_failures >= 3 {
            if !self.memory_only {
                warn!("login event storage unavailable; delivery continues in memory only");
            }
            self.memory_only = true;
            if !self.queue.loaded {
                self.discarded += self.queue.load(Vec::new());
            }
        }
        self.result = if self.memory_only {
            "storage unavailable: memory only"
        } else {
            "storage failed"
        };
        self.storage_retry = now.saturating_add(if self.memory_only { 60_000 } else { 5_000 });
    }

    fn drive_at(&mut self, wallet: &Wallet, now: u64, endpoint: &str) {
        if let Some(mut task) = self.storage.take() {
            match task.complete() {
                None => self.storage = Some(task),
                Some(Ok(Stored::Loaded(stored))) => {
                    self.discarded += self.queue.load(stored);
                    self.storage_needs_load = false;
                    self.storage_failures = 0;
                    if self.memory_only {
                        self.result = "storage recovered";
                    }
                    self.memory_only = false;
                }
                Some(Ok(Stored::Saved(ids))) => {
                    self.queue.finish_save(true);
                    self.discarded += self.queue.observe_saved_ids(&ids);
                    self.storage_failures = 0;
                    if self.memory_only {
                        self.result = "storage recovered";
                    }
                    self.memory_only = false;
                }
                Some(Err(error)) => {
                    warn!("login event storage failed: {error}");
                    self.storage_failed(now);
                }
            }
        }
        self.discarded += self.queue.expire(now);
        if self.storage.is_none() && now >= self.storage_retry {
            if self.storage_needs_load {
                self.storage = Some(IoTaskPool::get().spawn_compat(async {
                    let json = storage_timeout(platform::client_logins_read()).await?;
                    serde_json::from_str(&json)
                        .map(Stored::Loaded)
                        .map_err(|e| e.to_string())
                }));
            } else if let Some(events) = self.queue.begin_save() {
                self.storage = Some(IoTaskPool::get().spawn_compat(async move {
                    let json = serde_json::to_string(&events).map_err(|e| e.to_string())?;
                    let receipt = storage_timeout(platform::client_logins_write(json)).await?;
                    serde_json::from_str(&receipt)
                        .map(Stored::Saved)
                        .map_err(|e| e.to_string())
                }));
            }
        }
        let Some(active) = self.active.as_deref() else {
            return;
        };
        if wallet.is_guest()
            || wallet.address().map(|a| format!("{a:#x}")).as_deref() != Some(active)
        {
            self.pause();
            return;
        }
        if let Some(mut task) = self.task.take() {
            let Some(result) = task.complete() else {
                self.task = Some(task);
                return;
            };
            match result {
                AttemptResult::Retry | AttemptResult::EndpointUnavailable => {
                    self.result = "retrying";
                    self.next_attempt = now.saturating_add(retry_delay_ms(
                        if result == AttemptResult::EndpointUnavailable {
                            9
                        } else {
                            self.attempts
                        },
                        uuid::Uuid::new_v4().as_u128() as u64,
                    ));
                }
                AttemptResult::Accepted | AttemptResult::Rejected => {
                    if let Some(id) = self.event_id.as_deref() {
                        self.queue.remove(id);
                    }
                    self.result = if result == AttemptResult::Accepted {
                        "accepted"
                    } else {
                        "rejected"
                    };
                    self.event_id = None;
                    self.attempts = 0;
                    self.next_attempt = 0;
                }
            }
        }
        if !self.memory_only && !self.queue.ready() {
            return;
        }
        let Some(event) = self
            .queue
            .events
            .iter()
            .find(|event| event.eligible(active, endpoint, now))
        else {
            return;
        };
        if self.event_id.as_ref() != Some(&event.login_id) {
            self.event_id = Some(event.login_id.clone());
            self.attempts = 0;
            self.next_attempt = 0;
        }
        if now < self.next_attempt {
            return;
        }
        let event = event.clone();
        self.attempts = self.attempts.saturating_add(1);
        let wallet = wallet.snapshot();
        self.task = Some(IoTaskPool::get().spawn_compat(async move {
            // Includes remote signing and HTTP headers; login never waits for this work.
            async_std::future::timeout(
                Duration::from_secs(10),
                send(&wallet, &event, reqwest_client()),
            )
            .await
            .unwrap_or(AttemptResult::Retry)
        }));
    }
}

async fn send(
    wallet: &Wallet,
    event: &PendingClientLogin,
    client: reqwest::Client,
) -> AttemptResult {
    if event.endpoint.parse::<reqwest::Url>().is_err() {
        return AttemptResult::Rejected;
    }
    let Ok(headers) =
        wallet::sign_service_request("post", &event.endpoint, wallet, event.metadata()).await
    else {
        return AttemptResult::Retry;
    };
    let mut request = client.post(&event.endpoint);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            #[cfg(not(target_arch = "wasm32"))]
            drain(response).await;
            if status == 204 {
                AttemptResult::Accepted
            } else if matches!(status, 404 | 405) {
                warn!(
                    status,
                    "login event endpoint unavailable; retaining event for retry"
                );
                AttemptResult::EndpointUnavailable
            } else if retryable_status(status) {
                AttemptResult::Retry
            } else {
                warn!(status, "login event rejected");
                AttemptResult::Rejected
            }
        }
        Err(_) => AttemptResult::Retry,
    }
}

// Native reqwest needs body consumption for reuse; browser fetch manages its connections.
#[cfg(not(target_arch = "wasm32"))]
async fn drain(mut response: reqwest::Response) {
    // Bound error bodies and wall time; small responses return connections to the pool.
    let _ = async_std::future::timeout(Duration::from_secs(1), async {
        let mut bytes = 0;
        while let Ok(Some(chunk)) = response.chunk().await {
            bytes += chunk.len();
            if bytes >= 16 * 1024 {
                break;
            }
        }
    })
    .await;
}

async fn storage_timeout<T>(
    future: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    async_std::future::timeout(Duration::from_secs(10), future)
        .await
        .map_err(|_| "login event storage timed out".to_owned())?
}

#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/client_login_state")]
struct ClientLoginDebug;

fn debug(mut cmd: ConsoleCommand<ClientLoginDebug>, state: Res<ClientLoginDelivery>) {
    let Some(Ok(_)) = cmd.take() else { return };
    cmd.reply_ok(
        serde_json::json!({
            "active": state.active.is_some(), "pending": state.queue.events.len(),
            "inFlight": state.task.is_some(), "attempts": state.attempts,
            "result": if state.result.is_empty() { "idle" } else { state.result },
            "discarded": state.discarded, "durable": !state.memory_only && state.queue.ready(),
            "memoryOnly": state.memory_only, "storageFailures": state.storage_failures,
        "storageInFlight": state.storage.is_some(), "retryInMs": state.next_attempt.saturating_sub(now_ms())
        })
        .to_string(),
    );
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn signed_wallet() -> Wallet {
        let root = wallet::local_guest::new_root_key();
        let (ephemeral, delegates) =
            wallet::local_guest::sign_identity(&root, now_ms() as i64).unwrap();
        let mut wallet = Wallet::default();
        wallet.finalize(root.address(), ephemeral, delegates, false);
        wallet
    }

    fn pending_state(wallet: &Wallet, endpoint: &str) -> ClientLoginDelivery {
        IoTaskPool::get_or_init(Default::default);
        let mut state = ClientLoginDelivery::default();
        state.active = Some(format!("{:#x}", wallet.address().unwrap()));
        state.queue.push(PendingClientLogin {
            wallet: state.active.clone().unwrap(),
            endpoint: endpoint.into(),
            login_id: uuid::Uuid::new_v4().to_string(),
            occurred_at: now_ms(),
        });
        // Tests control storage completion without writing to the user's profile.
        state.storage_retry = u64::MAX;
        state
    }

    fn poll_until(
        state: &mut ClientLoginDelivery,
        wallet: &Wallet,
        endpoint: &str,
        done: impl Fn(&ClientLoginDelivery) -> bool,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done(state) {
            assert!(
                std::time::Instant::now() < deadline,
                "delivery did not finish"
            );
            state.drive_at(wallet, now_ms(), endpoint);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn drive_persists_before_signed_http_and_handles_ack_rejection_and_deployment_order() {
        for (status, memory_only) in [
            (204, false),
            (400, false),
            (404, false),
            (405, false),
            (204, true),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}/v1/client-login", listener.local_addr().unwrap());
            let wallet = signed_wallet();
            let mut state = pending_state(&wallet, &endpoint);
            state.drive_at(&wallet, now_ms(), &endpoint);
            assert!(state.task.is_none(), "HTTP must wait for load");
            assert!(listener.accept().is_err());
            state.queue.load(Vec::new());
            state.storage_needs_load = false;
            state.queue.begin_save().unwrap();
            state.drive_at(&wallet, now_ms(), &endpoint);
            assert!(
                state.task.is_none(),
                "HTTP must wait for save acknowledgment"
            );
            state.queue.finish_save(false);
            state.drive_at(&wallet, now_ms(), &endpoint);
            assert!(state.task.is_none(), "failed persistence cannot start HTTP");
            if memory_only {
                for _ in 0..3 {
                    state.storage_failed(now_ms());
                }
                assert!(state.memory_only);
                assert!(!state.queue.ready());
            } else {
                state.queue.begin_save().unwrap();
                state.queue.finish_save(true);
            }
            listener.set_nonblocking(false).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let body = if status == 204 {
                    ""
                } else {
                    "private server detail"
                };
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                String::from_utf8(bytes).unwrap()
            });
            state.drive_at(&wallet, now_ms(), &endpoint);
            poll_until(&mut state, &wallet, &endpoint, |s| s.task.is_none());
            let request = server.join().unwrap();
            assert!(request.contains("x-identity-metadata:"));
            if status == 204 || status == 400 {
                assert!(state.queue.events.is_empty());
                assert_eq!(
                    state.result,
                    if status == 204 {
                        "accepted"
                    } else {
                        "rejected"
                    }
                );
                assert_eq!(state.queue.begin_save().unwrap().removals.len(), 1);
            } else {
                assert_eq!(state.queue.events.len(), 1);
                assert_eq!(state.result, "retrying");
                assert!((149_000..=300_000).contains(&state.next_attempt.saturating_sub(now_ms())));
                let expiry = state.queue.events[0].occurred_at + common::client_login::MAX_AGE_MS;
                state.drive_at(&wallet, expiry, &endpoint);
                assert!(state.queue.events.is_empty());
                assert!(state.task.is_none());
            }
        }
    }

    #[test]
    fn drive_switch_cancels_inflight_ack_without_removing_another_accounts_event() {
        for disconnect in [false, true] {
            let mut wallet = signed_wallet();
            let endpoint = "https://example.org/v1/client-login";
            let mut state = pending_state(&wallet, endpoint);
            state.event_id = Some(state.queue.events[0].login_id.clone());
            state.task = Some(IoTaskPool::get().spawn_compat(async { AttemptResult::Accepted }));
            if disconnect {
                wallet.disconnect();
            } else {
                wallet = signed_wallet();
            }
            state.drive_at(&wallet, now_ms(), endpoint);
            assert!(state.task.is_none());
            assert!(state.active.is_none());
            assert_eq!(state.queue.events.len(), 1);
            assert_eq!(state.result, "paused");
        }
    }

    #[test]
    fn drive_storage_failures_enable_visible_memory_fallback_and_recover_unsaved_delta() {
        let wallet = signed_wallet();
        let endpoint = "https://example.org/v1/client-login";
        let mut state = pending_state(&wallet, endpoint);
        // Keep delivery off while injecting actual asynchronous storage results.
        state.active = None;
        for attempt in 1..=3 {
            state.storage =
                Some(IoTaskPool::get().spawn_compat(async { Err("storage unavailable".into()) }));
            poll_until(&mut state, &wallet, endpoint, |s| {
                s.storage_failures == attempt
            });
            assert_eq!(state.memory_only, attempt == 3);
            assert_eq!(state.queue.events.len(), 1);
            assert!(state.task.is_none());
        }
        assert!(state.queue.loaded);
        assert!(!state.queue.ready());
        assert_eq!(state.result, "storage unavailable: memory only");
        let original = state.queue.events[0].clone();
        state.storage =
            Some(IoTaskPool::get().spawn_compat(async { Ok(Stored::Loaded(Vec::new())) }));
        poll_until(&mut state, &wallet, endpoint, |s| !s.memory_only);
        assert_eq!(
            state.queue.begin_save().unwrap().additions,
            [original.clone()]
        );
        state.storage = Some(
            IoTaskPool::get()
                .spawn_compat(async move { Ok(Stored::Saved(vec![original.login_id])) }),
        );
        poll_until(&mut state, &wallet, endpoint, |s| s.queue.ready());
        assert_eq!(state.queue.events.len(), 1);
        assert_eq!(state.storage_failures, 0);
    }

    #[test]
    fn every_identity_completion_queues_without_a_social_consumer() {
        let root = wallet::local_guest::new_root_key();
        let (ephemeral, delegates) =
            wallet::local_guest::sign_identity(&root, now_ms() as i64).unwrap();
        let mut wallet = Wallet::default();
        wallet.finalize(root.address(), ephemeral, delegates, false);
        let mut state = ClientLoginDelivery::default();
        state.completed(&wallet);
        let first = state.queue.events[0].clone();
        state.completed(&wallet);
        assert_eq!(state.queue.events.len(), 2);
        assert!(state.queue.events.contains(&first));
        assert!(state
            .queue
            .events
            .iter()
            .any(|event| event.login_id != first.login_id));
        assert!(!state.queue.ready());
        state.pause();
        assert!(state.active.is_none());
        assert!(state.task.is_none());
        assert_eq!(state.queue.events.len(), 2);
    }

    #[test]
    fn gateway_login_signs_the_forwarded_path_and_sends_the_public_path() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let gateway = format!("http://gateway.example.test:{}", address.port());
        base_domain::set_gateway(&gateway).unwrap();
        let endpoint = base_domain::url(Service::SocialApi, "/v1/client-login");
        assert_eq!(endpoint, format!("{gateway}/social-api/v1/client-login"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            write!(
                stream,
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            String::from_utf8(bytes).unwrap()
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve("gateway.example.test", address)
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let mut wallet = Wallet::default();
        wallet.finalize_as_guest();
        let event = PendingClientLogin {
            wallet: format!("{:#x}", wallet.address().unwrap()),
            endpoint,
            login_id: uuid::Uuid::new_v4().to_string(),
            occurred_at: now_ms(),
        };
        async_std::task::block_on(platform::compat(async {
            assert_eq!(send(&wallet, &event, client).await, AttemptResult::Accepted);
        }));
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /social-api/v1/client-login HTTP/1.1\r\n"));
        let headers = request
            .lines()
            .filter_map(|line| line.split_once(": "))
            .map(|(key, value)| (key.to_lowercase(), value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        let signed: serde_json::Value =
            serde_json::from_str(&headers["x-identity-auth-chain-1"]).unwrap();
        assert_eq!(
            signed["payload"],
            format!(
                "post:/v1/client-login:{}:{}",
                headers["x-identity-timestamp"],
                event.metadata()
            )
        );
    }

    #[test]
    fn retry_resigns_same_event_and_keeps_snapshot_account() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!(
            "http://{}/social-api/v1/client-login",
            listener.local_addr().unwrap()
        );
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for status in [503, 204] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                requests.push(String::from_utf8(bytes).unwrap());
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
            requests
        });
        let mut live = Wallet::default();
        live.finalize_as_guest();
        let address = format!("{:#x}", live.address().unwrap());
        let wallet = live.snapshot();
        live.finalize_as_guest();
        live.disconnect();
        let event = PendingClientLogin {
            wallet: address.clone(),
            endpoint,
            login_id: uuid::Uuid::new_v4().to_string(),
            occurred_at: now_ms(),
        };
        async_std::task::block_on(platform::compat(async {
            assert_eq!(
                send(&wallet, &event, reqwest_client()).await,
                AttemptResult::Retry
            );
            async_std::task::sleep(Duration::from_millis(2)).await;
            assert_eq!(
                send(&wallet, &event, reqwest_client()).await,
                AttemptResult::Accepted
            );
        }));
        let requests = server.join().unwrap();
        let headers = requests
            .iter()
            .map(|request| {
                assert!(request.starts_with("POST /social-api/v1/client-login HTTP/1.1\r\n"));
                request
                    .lines()
                    .filter_map(|line| line.split_once(": "))
                    .map(|(key, value)| (key.to_lowercase(), value.to_string()))
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(headers[0]["x-identity-metadata"], event.metadata());
        assert_eq!(
            headers[0]["x-identity-metadata"],
            headers[1]["x-identity-metadata"]
        );
        assert_ne!(
            headers[0]["x-identity-timestamp"],
            headers[1]["x-identity-timestamp"]
        );
        for header in &headers {
            let signer: serde_json::Value =
                serde_json::from_str(&header["x-identity-auth-chain-0"]).unwrap();
            assert_eq!(signer["payload"], address);
            let signed: serde_json::Value =
                serde_json::from_str(&header["x-identity-auth-chain-1"]).unwrap();
            assert_eq!(
                signed["payload"],
                format!(
                    "post:/social-api/v1/client-login:{}:{}",
                    header["x-identity-timestamp"],
                    event.metadata()
                )
            );
        }
    }
}

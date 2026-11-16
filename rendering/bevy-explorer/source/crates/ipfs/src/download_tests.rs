use super::*;
use crate::{
    failed_remotes::Repeat, ipfs_path::IpfsType, ContentMap, EndpointConfig, IpfsEntity,
    RealmConfig, ServerAbout,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::Notify,
    time::timeout,
};

const LIMIT: Duration = Duration::from_secs(10);
const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx";
const BUSY: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const UNAVAILABLE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

struct Server(TcpListener);

impl Server {
    async fn start() -> Self {
        Self(TcpListener::bind("127.0.0.1:0").await.unwrap())
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/{path}", self.0.local_addr().unwrap())
    }

    /// The next request, which must be for `path`, and its header names in lower case.
    async fn request(&self, path: &str) -> (TcpStream, Vec<String>) {
        let (socket, _) = self.0.accept().await.unwrap();
        let mut request = BufReader::new(socket);
        let mut line = String::new();
        request.read_line(&mut line).await.unwrap();
        assert_eq!(
            line.split_whitespace().nth(1),
            Some(format!("/{path}").as_str())
        );
        let mut names = Vec::new();
        loop {
            line.clear();
            assert!(request.read_line(&mut line).await.unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, _)) = line.split_once(':') {
                names.push(name.trim().to_ascii_lowercase());
            }
        }
        (request.into_inner(), names)
    }

    async fn next(&self, path: &str) -> TcpStream {
        self.request(path).await.0
    }

    async fn reply(&self, path: &str, response: &[u8]) {
        self.next(path).await.write_all(response).await.unwrap();
    }
}

fn ipfs() -> Arc<IpfsIo> {
    IoTaskPool::get_or_init(bevy::tasks::TaskPool::new);
    Arc::new(IpfsIo::new(
        false,
        Box::new(FileAssetReader::new("assets")),
        None,
        Default::default(),
        1,
        #[cfg(feature = "ipfs_debug")]
        tokio::sync::mpsc::unbounded_channel().0,
    ))
}

fn asset(
    io: &Arc<IpfsIo>,
    server: &Server,
    path: &str,
    owner: Option<&str>,
) -> tokio::task::JoinHandle<Result<SharedBytes, AssetReaderError>> {
    tokio::spawn(io.clone().download(
        1,
        path.into(),
        server.url(path),
        None,
        owner.map(str::to_owned),
    ))
}

fn shared(io: &Arc<IpfsIo>, server: &Server, path: &str, owner: &str) -> SharedReader {
    let (reader, task) = io.share_download(
        CacheKey::Url(path.to_owned()),
        1,
        path.into(),
        server.url(path),
        Some(owner.to_owned()),
    );
    if let Some(task) = task {
        IoTaskPool::get().spawn_compat(task).detach();
    }
    reader
}

async fn until(condition: impl Fn() -> bool) {
    while !condition() {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn retry_uses_live_priorities_after_another_download_completes_in_backoff() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        io.register_download_priority("retry", LoadPriority::TOP);
        io.register_download_priority("competitor", LoadPriority::SCENE_PENDING);
        io.register_download_priority("first-reader", LoadPriority::TOP);
        let (first_reader, attempts) = DownloadReaders::first(
            &io.request_slots,
            Some("first-reader".to_owned()),
            None,
            None,
        );
        let owners = first_reader.readers.clone();
        let sleeping = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let retry = {
            let io = io.clone();
            let sleeping = sleeping.clone();
            let resume = resume.clone();
            let request = io.client.get(server.url("retry")).build().unwrap();
            tokio::spawn(async move {
                io.fetch_request_inner(
                    attempts,
                    request,
                    &io.client,
                    FetchPolicy {
                        headers_timeout: LIMIT,
                        idle_timeout: LIMIT,
                        max_attempts: 4,
                    },
                    None,
                    |delay| {
                        assert_eq!(delay, Duration::from_secs(60));
                        sleeping.notify_one();
                        resume.notified()
                    },
                )
                .await
            })
        };

        server.reply("retry", BUSY).await;
        sleeping.notified().await;
        assert!(!io.request_slots.has_waiters());
        let other = asset(&io, &server, "during-backoff", None);
        server.reply("during-backoff", OK).await;
        assert_eq!(other.await.unwrap().unwrap().as_ref(), b"x");

        let blocker = io.request_slots.try_acquire().unwrap();
        let unowned = asset(&io, &server, "unowned", None);
        until(|| io.request_slots.waiting() == 1).await;
        let older = asset(&io, &server, "older-in-tier", Some("retry"));
        until(|| io.request_slots.waiting() == 2).await;
        // A different scene needs these bytes during backoff. Its retry must re-enter behind
        // older-in-tier but ahead of unowned, without keeping first-reader's top rank.
        let _reader = owners.try_register(Some("retry".to_owned())).unwrap();
        drop(first_reader);
        resume.notify_one();
        until(|| io.request_slots.waiting() == 3).await;
        let competitor = asset(&io, &server, "competitor", Some("competitor"));
        until(|| io.request_slots.waiting() == 4).await;
        // The retry is both older and initially better. Only the new keys give the competitor this grant.
        io.update_download_priorities([
            ("retry", LoadPriority::SCENE_PENDING),
            ("competitor", LoadPriority::TOP),
        ]);
        drop(blocker);
        server.reply("competitor", OK).await;
        server.reply("older-in-tier", OK).await;
        server.reply("retry", OK).await;
        server.reply("unowned", OK).await;
        assert_eq!(older.await.unwrap().unwrap().as_ref(), b"x");
        assert_eq!(unowned.await.unwrap().unwrap().as_ref(), b"x");
        assert_eq!(competitor.await.unwrap().unwrap().as_ref(), b"x");
        assert!(matches!(retry.await.unwrap(), Ok(response) if response.body == b"x"));
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 6);
        assert_eq!(io.request_metrics.retries.load(Ordering::Relaxed), 1);
        assert_eq!(
            io.request_metrics.status_failures.load(Ordering::Relaxed),
            1
        );
        assert_eq!(io.request_metrics.bytes.load(Ordering::Relaxed), 5);
        let downloads = io.download_stats();
        assert_eq!(downloads.bytes, 4, "only completed assets count here");
        assert!(downloads.mean_first_byte_ms().is_some());
        assert!(!io.request_slots.has_waiters());
        assert!(io.request_slots.try_acquire().is_some());
    })
    .await
    .expect("priority retry did not finish");
}

#[tokio::test]
async fn preview_delay_leaves_the_slot_free_and_shared_readers_can_cancel() {
    timeout(LIMIT, async {
        let mut io = ipfs();
        Arc::get_mut(&mut io).unwrap().is_preview = true;
        io.preview_delay_ms.store(60_000, Ordering::Relaxed);
        let server = Server::start().await;
        let first = shared(&io, &server, "preview", "first");
        let second = shared(&io, &server, "preview", "second");
        assert!(Arc::ptr_eq(&first.download, &second.download));
        assert_eq!(io.request_slots.available_permits(), 1);
        drop(first);
        assert_eq!(io.in_flight.lock().unwrap().len(), 1);
        drop(second);
        until(|| io.in_flight.lock().unwrap().is_empty()).await;
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 0);
        assert_eq!(io.request_slots.available_permits(), 1);
    })
    .await
    .expect("delayed download survived its last reader");
}

#[tokio::test]
async fn a_503_without_retry_after_is_retried_after_a_growing_wait() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let (_reader, attempts) = DownloadReaders::first(&io.request_slots, None, None, None);
        let waits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fetched = {
            let io = io.clone();
            let waits = waits.clone();
            let request = io.client.get(server.url("flaky")).build().unwrap();
            tokio::spawn(async move {
                io.fetch_request_inner(
                    attempts,
                    request,
                    &io.client,
                    FetchPolicy {
                        headers_timeout: LIMIT,
                        idle_timeout: LIMIT,
                        max_attempts: 4,
                    },
                    None,
                    move |delay| {
                        waits.lock().unwrap().push(delay);
                        async {}
                    },
                )
                .await
            })
        };

        server.reply("flaky", UNAVAILABLE).await;
        server.reply("flaky", UNAVAILABLE).await;
        server.reply("flaky", OK).await;
        assert!(matches!(fetched.await.unwrap(), Ok(response) if response.body == b"x"));
        assert_eq!(
            *waits.lock().unwrap(),
            [Duration::from_millis(500), Duration::from_millis(1000)]
        );
    })
    .await
    .expect("the 503 retries did not finish");
}

#[tokio::test]
async fn a_content_fetch_uses_exactly_one_ticket_on_its_own_gate() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let previous = io.request_slots.ticket(None).seq;
        let request = io.client.get(server.url("content")).build().unwrap();
        let (content, ()) = tokio::join!(
            io.fetch_request(request, io.client.clone(), LIMIT, LIMIT, None),
            server.reply("content", OK),
        );
        assert!(matches!(content, Ok(response) if response.body == b"x"));
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 1);
        assert_eq!(io.request_slots.ticket(None).seq, previous + 2);
        assert!(io.request_slots.try_acquire().is_some());
    })
    .await
    .expect("a content fetch blocked on its single slot");
}

#[tokio::test]
async fn dropping_shared_readers_withdraws_only_the_last_claim_of_each_owner() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        io.register_download_priority("near", LoadPriority::TOP);
        io.register_download_priority("mid", LoadPriority::SCENE_PENDING);
        let blocker = io.request_slots.try_acquire().unwrap();
        let far = shared(&io, &server, "shared", "far");
        until(|| io.request_slots.waiting() == 1).await;
        let near1 = shared(&io, &server, "shared", "near");
        let near2 = shared(&io, &server, "shared", "near");
        let mid = asset(&io, &server, "mid", Some("mid"));
        until(|| io.request_slots.waiting() == 3).await;
        drop(near1);
        assert_eq!(io.request_slots.waiting(), 3);
        drop(near2);
        assert_eq!(io.request_slots.waiting(), 2);
        drop(blocker);
        server.reply("mid", OK).await;
        server.reply("shared", OK).await;
        assert_eq!(mid.await.unwrap().unwrap().as_ref(), b"x");
        let mut result = far.result();
        assert!(matches!(&*result.wait_for(Option::is_some).await.unwrap(), Some(Ok(bytes)) if bytes.as_ref() == b"x"));
        assert!(io.in_flight.lock().unwrap().is_empty());
    }).await.expect("shared reader priority did not settle");
}

#[tokio::test]
async fn last_shared_reader_cancels_a_queued_attempt_and_a_retry_timer() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let blocker = io.request_slots.try_acquire().unwrap();
        let queued = shared(&io, &server, "queued", "scene");
        until(|| io.request_slots.has_waiters()).await;
        let queued_handle = queued.download.clone();
        drop(queued);
        until(|| !io.request_slots.has_waiters() && io.in_flight.lock().unwrap().is_empty()).await;
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 0);
        drop(queued_handle);
        drop(blocker);

        let sleeping = shared(&io, &server, "sleeping", "scene");
        server.reply("sleeping", BUSY).await;
        until(|| io.request_metrics.status_failures.load(Ordering::Relaxed) == 1).await;
        let sleeping_handle = sleeping.download.clone();
        drop(sleeping);
        until(|| io.in_flight.lock().unwrap().is_empty()).await;
        drop(sleeping_handle);
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 1);
        assert_eq!(io.request_metrics.retries.load(Ordering::Relaxed), 0);
        assert!(io.request_slots.try_acquire().is_some());
    })
    .await
    .expect("a cancelled shared download retained its entry or permit");
}

#[tokio::test]
async fn immediate_timeout_retry_yields_to_an_older_unowned_request() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let request = io.client.get(server.url("retry")).build().unwrap();
        let blocker = io.request_slots.try_acquire().unwrap();
        let caller = io.clone();
        let retry = tokio::spawn(async move {
            caller
                .fetch_request(
                    request,
                    caller.client.clone(),
                    Duration::from_millis(500),
                    LIMIT,
                    None,
                )
                .await
        });
        until(|| io.request_slots.waiting() == 1).await;
        let other = asset(&io, &server, "waiting", None);
        until(|| io.request_slots.waiting() == 2).await;
        drop(blocker);
        let stalled = server.next("retry").await;
        server.reply("waiting", OK).await;
        server.reply("retry", OK).await;
        assert_eq!(other.await.unwrap().unwrap().as_ref(), b"x");
        assert!(matches!(retry.await.unwrap(), Ok(response) if response.body == b"x"));
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 3);
        assert_eq!(io.request_metrics.retries.load(Ordering::Relaxed), 1);
        drop(stalled);
    })
    .await
    .expect("immediate timeout retry did not rejoin the unowned queue");
}

#[tokio::test]
async fn a_stale_upgrade_cannot_revive_a_download_after_its_last_reader_left() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let old = shared(&io, &server, "same", "first-scene");
        server.reply("same", BUSY).await;
        until(|| io.request_metrics.status_failures.load(Ordering::Relaxed) == 1).await;
        let blocker = io.request_slots.try_acquire().unwrap();

        // A lookup upgraded the weak reference just before the last real reader left.
        let stale = old.download.clone();
        let mut old_result = old.result();
        drop(old);
        let (new, task) = io.share_download(CacheKey::Url("same".into()), 2, "same".into(),
            server.url("same"), Some("next-scene".into()));
        IoTaskPool::get().spawn_compat(task.expect("a generation with zero readers is terminal")).detach();
        assert!(!Arc::ptr_eq(&new.download, &stale));
        // A stale lookup can still hold the task handle when the last real reader goes.
        assert!(old_result.changed().await.is_err());
        drop(stale);
        until(|| io.request_slots.has_waiters()).await;
        assert_eq!(io.in_flight.lock().unwrap().len(), 1, "old cleanup must preserve the replacement");
        drop(blocker);
        server.reply("same", OK).await;
        let mut result = new.result();
        assert!(matches!(&*result.wait_for(Option::is_some).await.unwrap(), Some(Ok(bytes)) if bytes.as_ref() == b"x"));
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 2);
        assert_eq!(io.request_metrics.retries.load(Ordering::Relaxed), 0);
    }).await.expect("replacement did not get its own attempt");
}

fn cache_fixture(label: &str) -> (Arc<IpfsIo>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "ipfs-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let mut io = ipfs();
    Arc::get_mut(&mut io).unwrap().default_fs_path = Some(dir.clone());
    (io, dir)
}

#[tokio::test]
async fn cache_writer_skips_a_stale_staging_file() {
    let (io, dir) = cache_fixture("stale-staging");
    let stale = dir.join(format!("asset.part-{}-0", std::process::id()));
    std::fs::write(&stale, b"other writer").unwrap();
    io.cache_bytes("asset", b"verified").await.unwrap();
    assert_eq!(std::fs::read(dir.join("asset")).unwrap(), b"verified");
    assert_eq!(std::fs::read(&stale).unwrap(), b"other writer");
    std::fs::remove_file(dir.join("asset")).unwrap();
    std::fs::remove_file(stale).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[tokio::test]
async fn failed_cache_publish_removes_its_staging_file() {
    let (io, dir) = cache_fixture("failed-publish");
    let destination = dir.join("blocked");
    std::fs::create_dir(&destination).unwrap();
    assert!(io.cache_bytes("blocked", b"bytes").await.is_err());
    std::fs::remove_dir(destination).unwrap();
    // Reject a left-over staging file even when rename failed.
    std::fs::remove_dir(dir).unwrap();
}

#[tokio::test]
async fn concurrent_writes_to_one_cache_key_publish_complete_files() {
    let (io, dir) = cache_fixture("concurrent-staging");
    let first = vec![1; 65536];
    let second = vec![2; 65536];
    let (a, b) = tokio::join!(
        io.cache_bytes("shared", &first),
        io.cache_bytes("shared", &second)
    );
    a.unwrap();
    b.unwrap();
    let cached = std::fs::read(dir.join("shared")).unwrap();
    assert!(cached == first || cached == second);
    std::fs::remove_file(dir.join("shared")).unwrap();
    // Every writer cleaned up its staging file.
    std::fs::remove_dir(dir).unwrap();
}

#[tokio::test]
async fn shared_downloads_keep_arrival_order_when_workers_poll_in_reverse() {
    use std::future::{poll_fn, Future};
    use std::task::Poll;

    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let share = |name: &str| {
            io.share_download(
                CacheKey::Url(name.into()),
                1,
                name.into(),
                server.url(name),
                Some("scene".into()),
            )
        };
        let (_first_reader, first) = share("first");
        let (_second_reader, second) = share("second");
        let mut second = Box::pin(second.unwrap());
        poll_fn(|cx| {
            assert!(second.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::join!(first.unwrap(), second, async {
            server.reply("first", OK).await;
            server.reply("second", OK).await;
        });
    })
    .await
    .expect("shared downloads did not finish in FIFO order");
}

fn status_only(status: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
}

/// A content download keyed by `key`, run to its failure.
async fn fail_shared(
    io: &Arc<IpfsIo>,
    server: &Server,
    path: &str,
    key: CacheKey,
    status: &str,
) -> AssetReaderError {
    let (reader, task) = io.share_download(key, 1, path.into(), server.url(path), None);
    IoTaskPool::get().spawn_compat(task.unwrap()).detach();
    server.reply(path, &status_only(status)).await;
    let mut result = reader.result();
    let result = result
        .wait_for(Option::is_some)
        .await
        .unwrap()
        .clone()
        .unwrap();
    result.err().expect("the download failed")
}

#[tokio::test]
async fn a_content_hash_that_answers_a_client_error_is_remembered_for_the_session() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let later = Instant::now() + Duration::from_secs(3600);
        for (path, status, code) in [
            ("missing", "404 Not Found", 404),
            ("gone", "410 Gone", 410),
            ("bad", "400 Bad Request", 400),
        ] {
            let key = CacheKey::Content(format!("bafk{path}"));
            let error = fail_shared(&io, &server, path, key, status).await;
            assert!(error.to_string().contains(&code.to_string()), "{error}");
            let repeat = io
                .failed_remotes
                .lock()
                .unwrap()
                .repeat(&server.url(path), later, true);
            assert_eq!(
                repeat,
                Some(Repeat::Session(
                    reqwest::StatusCode::from_u16(code).unwrap()
                )),
                "{path}"
            );
        }
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 3);
    })
    .await
    .expect("definitive failures were not remembered");
}

#[tokio::test]
async fn transient_and_url_keyed_failures_keep_the_retry_window() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let mut urls = Vec::new();
        for (path, status) in [
            ("timeout", "408 Request Timeout"),
            ("busy", "429 Too Many Requests"),
            ("forbidden", "403 Forbidden"),
        ] {
            let key = CacheKey::Content(format!("bafk{path}"));
            fail_shared(&io, &server, path, key, status).await;
            urls.push(server.url(path));
        }
        fail_shared(
            &io,
            &server,
            "missing-url",
            CacheKey::Url("missing-url".into()),
            "404 Not Found",
        )
        .await;
        urls.push(server.url("missing-url"));
        let uncached = asset(&io, &server, "uncached", None);
        server
            .reply("uncached", &status_only("404 Not Found"))
            .await;
        assert!(uncached.await.unwrap().is_err());
        urls.push(server.url("uncached"));

        let mut failed = io.failed_remotes.lock().unwrap();
        for url in &urls {
            assert_eq!(
                failed.repeat(url, Instant::now(), false),
                Some(Repeat::Recent),
                "{url}"
            );
            assert_eq!(
                failed.repeat(url, Instant::now() + Duration::from_secs(11), false),
                None,
                "{url}"
            );
        }
        assert_eq!(failed.session_len(), 0);
        drop(failed);
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 5);
    })
    .await
    .expect("transient failures were not kept for the retry window");
}

const SCENE: &str = "bafkreiscene";
const MISSING: &str = "bafkreimissing";

/// A realm on `server` with one scene whose `missing.png` is [`MISSING`]; returns its path.
async fn loopback_realm(io: &Arc<IpfsIo>, server: &Server) -> PathBuf {
    let about = ServerAbout {
        content: Some(EndpointConfig {
            healthy: true,
            public_url: format!("http://{}", server.0.local_addr().unwrap()),
        }),
        ..Default::default()
    };
    {
        let mut context = io.context.write().await;
        context.about = Some(about.clone());
        context.entities.insert(
            SCENE.to_owned(),
            IpfsEntity {
                collection: ContentMap::new_single("missing.png".to_owned(), MISSING.to_owned()),
                metadata: None,
            },
        );
    }
    io.realm_config_sender.send_replace(Some(RealmConfig {
        about_url: String::new(),
        address: String::new(),
        about,
        connected: true,
        secret: None,
        spawn_parcel: None,
    }));
    PathBuf::from(&IpfsPath::new(IpfsType::new_content_file(
        SCENE.to_owned(),
        "missing.png".to_owned(),
    )))
}

/// One read of the missing file, answered by `server` with a 404.
async fn read_missing(
    io: &Arc<IpfsIo>,
    server: &Server,
    path: &Path,
) -> Result<(), AssetReaderError> {
    let request = format!("contents/{MISSING}");
    let not_found = status_only("404 Not Found");
    let (read, ()) = tokio::join!(
        async { io.read(path).await.map(|_| ()) },
        server.reply(&request, &not_found)
    );
    read
}

#[tokio::test]
async fn a_preview_client_asks_again_for_a_missing_file_at_once() {
    timeout(LIMIT, async {
        let mut io = ipfs();
        Arc::get_mut(&mut io).unwrap().is_preview = true;
        let server = Server::start().await;
        let path = loopback_realm(&io, &server).await;
        for _ in 0..2 {
            assert!(read_missing(&io, &server, &path).await.is_err());
        }
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 2);
        assert_eq!(io.failed_remotes.lock().unwrap().session_len(), 0);
    })
    .await
    .expect("preview did not ask again");
}

#[tokio::test]
async fn a_preview_client_does_not_keep_a_content_failure_for_the_session() {
    timeout(LIMIT, async {
        let mut io = ipfs();
        Arc::get_mut(&mut io).unwrap().is_preview = true;
        let server = Server::start().await;
        let key = CacheKey::Content("bafkmissing".into());
        fail_shared(&io, &server, "missing", key, "404 Not Found").await;
        let mut failed = io.failed_remotes.lock().unwrap();
        assert_eq!(failed.session_len(), 0);
        assert_eq!(
            failed.repeat(&server.url("missing"), Instant::now(), false),
            Some(Repeat::Recent)
        );
    })
    .await
    .expect("preview kept a session failure");
}

fn seed_session_404(io: &Arc<IpfsIo>, server: &Server) {
    seed_session_404_at(io, server, Instant::now());
}

fn seed_session_404_at(io: &Arc<IpfsIo>, server: &Server, at: Instant) {
    let url = format!(
        "http://{}/contents/{MISSING}",
        server.0.local_addr().unwrap()
    );
    io.failed_remotes
        .lock()
        .unwrap()
        .record(url, Some(reqwest::StatusCode::NOT_FOUND), at);
}

#[tokio::test]
async fn a_session_failure_is_answered_by_read_without_a_request() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let path = loopback_realm(&io, &server).await;
        seed_session_404(&io, &server);
        // a loopback read has no content key, so the entry answers it only within the retry window
        let error = io.read(&path).await.map(|_| ()).unwrap_err();
        assert!(
            error.to_string().contains("repeat request for failed"),
            "{error}"
        );
        assert!(timeout(Duration::from_millis(200), server.0.accept())
            .await
            .is_err());
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 0);
    })
    .await
    .expect("a session failure was not answered from memory");
}

#[tokio::test]
async fn a_session_failure_answers_only_content_reads() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let path = loopback_realm(&io, &server).await;
        let long_ago = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        seed_session_404_at(&io, &server, long_ago);
        // a loopback read has no content key, so the entry is not its answer
        assert!(read_missing(&io, &server, &path).await.is_err());
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 1);
        let url = format!(
            "http://{}/contents/{MISSING}",
            server.0.local_addr().unwrap()
        );
        assert_eq!(
            io.failed_remotes
                .lock()
                .unwrap()
                .repeat(&url, Instant::now(), true),
            Some(Repeat::Session(reqwest::StatusCode::NOT_FOUND))
        );
    })
    .await
    .expect("a session failure answered a read that is not a content read");
}

#[tokio::test]
async fn a_successful_download_clears_a_failure_of_its_url() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let url = server.url("found");
        io.failed_remotes.lock().unwrap().record(
            url.clone(),
            Some(reqwest::StatusCode::NOT_FOUND),
            Instant::now(),
        );
        let download = asset(&io, &server, "found", None);
        server.reply("found", OK).await;
        assert!(download.await.unwrap().is_ok());
        let mut failed = io.failed_remotes.lock().unwrap();
        assert_eq!(failed.repeat(&url, Instant::now(), true), None);
        assert_eq!(failed.session_len(), 0);
    })
    .await
    .expect("a success did not clear the failure");
}

#[tokio::test]
async fn a_preview_client_ignores_a_session_failure() {
    timeout(LIMIT, async {
        let mut io = ipfs();
        Arc::get_mut(&mut io).unwrap().is_preview = true;
        let server = Server::start().await;
        let path = loopback_realm(&io, &server).await;
        seed_session_404(&io, &server);
        assert!(read_missing(&io, &server, &path).await.is_err());
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 1);
    })
    .await
    .expect("preview did not ask again");
}

#[tokio::test]
async fn a_loopback_404_is_answered_from_memory_only_within_the_retry_window() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let path = loopback_realm(&io, &server).await;
        assert!(read_missing(&io, &server, &path).await.is_err());
        let second = io.read(&path).await.map(|_| ()).unwrap_err();
        assert!(
            second.to_string().contains("repeat request for failed"),
            "{second}"
        );
        assert!(timeout(Duration::from_millis(200), server.0.accept())
            .await
            .is_err());
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 1);
        let url = format!(
            "http://{}/contents/{MISSING}",
            server.0.local_addr().unwrap()
        );
        assert_eq!(
            io.failed_remotes.lock().unwrap().repeat(
                &url,
                Instant::now() + Duration::from_secs(11),
                false
            ),
            None
        );
    })
    .await
    .expect("loopback failure was not answered from memory");
}

#[tokio::test]
async fn a_poisoned_failure_memory_keeps_answering() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        let path = loopback_realm(&io, &server).await;
        let poisoner = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _held = io.failed_remotes.lock().unwrap();
                    panic!("poison the failure memory");
                })
                .join()
        });
        assert!(poisoner.is_err());
        assert!(io.failed_remotes.is_poisoned());
        assert!(read_missing(&io, &server, &path).await.is_err());
        let second = io.read(&path).await.map(|_| ()).unwrap_err();
        assert!(
            second.to_string().contains("repeat request for failed"),
            "{second}"
        );
        assert_eq!(io.request_metrics.attempts.load(Ordering::Relaxed), 1);
    })
    .await
    .expect("a poisoned failure memory stopped answering");
}

// Pins the request builder that native and web share, not the web branch: this module only builds
// on native, so a header added again under `cfg(target_arch = "wasm32")` would not turn it red.
#[tokio::test]
async fn a_cacheable_download_does_not_ask_the_service_worker_to_look_in_its_cache() {
    timeout(LIMIT, async {
        let io = ipfs();
        let server = Server::start().await;
        for key in [
            CacheKey::Content("bafkreicontent".to_owned()),
            CacheKey::Url("urldigest".to_owned()),
        ] {
            let path = key.name().to_owned();
            let (reader, task) =
                io.share_download(key, 1, path.clone().into(), server.url(&path), None);
            IoTaskPool::get().spawn_compat(task.unwrap()).detach();
            let (mut stream, names) = server.request(&path).await;
            stream.write_all(OK).await.unwrap();
            let mut result = reader.result();
            let body = result
                .wait_for(Option::is_some)
                .await
                .unwrap()
                .clone()
                .unwrap()
                .unwrap();
            // the server saw the request and its headers were read
            assert_eq!(body.0.as_slice(), b"x");
            assert!(names.iter().any(|name| name == "host"), "{names:?}");
            // and it carried nothing that sends it through the worker's cache
            assert!(!names.iter().any(|name| name == "x-ipfs"), "{names:?}");
        }
    })
    .await
    .unwrap();
}

//! Root HTTP server lifecycle.
use crate::{comms, config::Config, jobs, shared};
use anyhow::Context;
use std::{sync::Arc, time::Duration};

/// How long the workers get to stop (flush, persist cursors) before they are aborted.
const JOBS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// `WRITES_RESTORE_STAMP_DIR`: from here on every write gate checks the
/// stamps live; the startup verdict is only logged, so a node that boots
/// with a stale stamp serves its reads and takes writes once a backup and a
/// drill land.
pub(super) fn install_restore_stamps(config: &Config) -> anyhow::Result<()> {
    let Some(dir) = config.restore_stamp_dir() else {
        tracing::info!("WRITES_RESTORE_STAMP_DIR unset: the write switches alone decide");
        return Ok(());
    };
    let stamps = shared::writes::RestoreStamps::from_settings(
        dir,
        config.writes.writes_backup_max_age_hours,
        config.writes.writes_drill_max_age_days,
    )
    .map_err(anyhow::Error::msg)?;
    let on = config.write_switches_on().join(",");
    match stamps.check() {
        Ok(()) => tracing::info!(dir, switches_on = %on, "restore stamps are fresh"),
        Err(hold) if on.is_empty() => {
            tracing::info!(dir, "restore stamps: {hold} (every write switch is off)")
        }
        Err(hold) => tracing::warn!(dir, switches_on = %on, "writes are held: {hold}"),
    }
    if !shared::writes::restore::install(stamps) {
        anyhow::bail!("the restore stamps were installed twice in one process");
    }
    Ok(())
}

pub(super) fn register_metrics() {
    comms::gatekeeper::admission::cluster::metrics::register_series();
}

pub(super) fn install_metrics(raw: &str) -> anyhow::Result<comms::metrics::PrometheusHandle> {
    let bind = comms::metrics::serve_metrics_bind(raw)?;
    let handle = comms::metrics::install_prometheus_exporter(bind)?;
    register_metrics();
    tracing::info!(%bind, "serve /metrics listening");
    Ok(handle)
}

/// Installs the shutdown signal handlers now and returns the wait for one of
/// them. Call it before logging that the process is ready, so a signal sent
/// in reply to that line finds its handler.
pub(crate) fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt()).expect("installing the SIGINT handler");
        let mut term = signal(SignalKind::terminate()).expect("installing the SIGTERM handler");
        async move {
            tokio::select! {
                _ = interrupt.recv() => tracing::info!("SIGINT received"),
                _ = term.recv() => tracing::info!("SIGTERM received"),
            }
        }
    }
    #[cfg(not(unix))]
    async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
    }
}

pub(super) async fn run_http(
    addr: String,
    app: axum::Router,
    connector_lifecycle: comms::archipelago::AppState,
    jobs: &Arc<jobs::Jobs>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr} (HTTP_SERVER_HOST + HTTP_SERVER_PORT)"))?;
    connector_lifecycle
        .started
        .store(true, std::sync::atomic::Ordering::Release);
    let shutdown = shutdown_signal();
    tracing::info!(%addr, "catalyst listening");
    let shutdown_jobs = jobs.clone();
    axum::serve(
        listener,
        axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
            std::net::SocketAddr,
        >(app),
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        comms::archipelago::shutdown(&connector_lifecycle).await;
        let aborted = shutdown_jobs.shutdown(JOBS_SHUTDOWN_TIMEOUT).await;
        if !aborted.is_empty() {
            tracing::warn!(?aborted, "jobs aborted at shutdown");
        }
        tracing::info!("workers stopped; closing the listener");
    })
    .await?;
    Ok(())
}

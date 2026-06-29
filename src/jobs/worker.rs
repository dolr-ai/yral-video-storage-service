//! Reusable async primitives for the leased background "sweep" worker.
//!
//! Three small building blocks consumed by the worker loop (built in a later
//! task): a pure "is discovery due?" check, a lease-heartbeat keep-alive
//! wrapper, and a compare-and-swap guard that skips work already in flight.

/// Pure predicate: has enough time elapsed since `last` to run discovery again?
///
/// Returns `true` when discovery has never run (`last == None`) or when at least
/// `interval` has elapsed between `last` and `now`. An `interval` too large to
/// represent as a `chrono::Duration` saturates to the maximum, so the comparison
/// stays well-defined.
pub fn discovery_due(
    last: Option<chrono::DateTime<chrono::Utc>>,
    interval: std::time::Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    match last {
        None => true,
        Some(l) => {
            let elapsed = now.signed_duration_since(l);
            elapsed >= chrono::Duration::from_std(interval).unwrap_or(chrono::Duration::MAX)
        }
    }
}

/// Run `fut` while a sibling task keeps the lease heartbeat fresh.
///
/// Spawns a background task that re-runs [`acquire_or_renew_lease`] every
/// `ttl / 3` so a long-running `fut` cannot let the lease lapse and be stolen by
/// a peer. The renew task is aborted as soon as `fut` resolves; `fut`'s output
/// is returned unchanged. `client` is shared via `Arc` so the renew task can use
/// it concurrently.
///
/// [`acquire_or_renew_lease`]: crate::media_index::acquire_or_renew_lease
pub async fn with_heartbeat_renew<F, T>(
    client: std::sync::Arc<tokio_postgres::Client>,
    owner: String,
    ttl: std::time::Duration,
    fut: F,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let renew_client = client.clone();
    let renew_owner = owner.clone();
    let renew = tokio::spawn(async move {
        let interval = ttl / 3;
        loop {
            tokio::time::sleep(interval).await;
            let _ =
                crate::media_index::acquire_or_renew_lease(&renew_client, &renew_owner, ttl).await;
        }
    });
    // Abort the renew task on ANY exit — including when this future is dropped
    // mid-flight (e.g. graceful-shutdown cancellation in the worker's `select!`).
    // A bare `JoinHandle` drop only *detaches* the task in tokio; the RAII guard
    // makes the abort unconditional so the renew loop can't outlive `fut`.
    struct RenewGuard(tokio::task::JoinHandle<()>);
    impl Drop for RenewGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _renew_guard = RenewGuard(renew);
    fut.await
}

/// Run `f`'s future only if `flag` is currently free, guarding against overlap.
///
/// Takes a *closure* rather than a prebuilt future so the guarded work is never
/// constructed when the flag is already held. Compare-exchanges `flag` from
/// `false` to `true`; if it was already `true`, returns `None` (skipped).
/// Otherwise runs the closure, returns `Some(result)`, and resets the flag to
/// `false` on drop (including on panic) via an RAII guard mirroring
/// [`JobGuard`](crate::jobs::JobGuard).
pub async fn cas_guarded<F, Fut, T>(
    flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    f: F,
) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use std::sync::atomic::Ordering;
    if flag
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return None; // already held
    }
    struct FlagGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for FlagGuard {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    let _guard = FlagGuard(flag.clone());
    Some(f().await)
}

/// One sweep pass over a single DB connection: elect via the lease, then (only if
/// this box owns it) drain eligible work and run discovery when due. The two side
/// effects — `drain` (hashing) and `discovery` (scan + import) — are injected as
/// closures so the control flow is unit-testable without real S3/ffmpeg.
///
/// Ordering:
/// 1. `acquire_or_renew_lease` — non-owners return early (no-op).
/// 2. Drain only when `any_eligible_for_hash` is true (missing AND not recently
///    failed), so an idle/dead-only fleet never inserts an empty job-run row or
///    re-downloads dead videos. Runs under heartbeat-renew + the per-box CAS guard.
/// 3. Discovery only when due by the persisted `last_discovery_at`; on a real run
///    (CAS not held by a manual import), persist the new cadence timestamp.
#[allow(clippy::too_many_arguments)]
pub async fn run_one_pass<DF, DFut, GF, GFut>(
    me: &str,
    client: std::sync::Arc<tokio_postgres::Client>,
    drain_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    import_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ttl: std::time::Duration,
    discovery_interval: std::time::Duration,
    failed_within: std::time::Duration,
    drain: DF,
    discovery: GF,
) -> Result<(), tokio_postgres::Error>
where
    DF: FnOnce() -> DFut,
    DFut: std::future::Future<Output = ()>,
    GF: FnOnce() -> GFut,
    GFut: std::future::Future<Output = ()>,
{
    if !crate::media_index::acquire_or_renew_lease(&client, me, ttl).await? {
        return Ok(());
    }

    let eligible = crate::media_index::any_eligible_for_hash(
        &client,
        phash::HASH_KIND,
        phash::HASH_VERSION,
        crate::jobs::media_phash::INPUT_MEDIA_VERSION,
        failed_within,
    )
    .await?;
    if eligible {
        with_heartbeat_renew(client.clone(), me.to_string(), ttl, async {
            cas_guarded(drain_flag, drain).await;
        })
        .await;
    }

    let last = crate::media_index::get_last_discovery_at(&client).await?;
    if discovery_due(last, discovery_interval, chrono::Utc::now()) {
        let ran = with_heartbeat_renew(client.clone(), me.to_string(), ttl, async {
            cas_guarded(import_flag, discovery).await
        })
        .await;
        // Only advance the cadence if discovery actually ran (CAS not held elsewhere).
        if ran.is_some() {
            crate::media_index::set_last_discovery_at(&client, me, chrono::Utc::now()).await?;
        }
    }

    Ok(())
}

/// Runtime dependencies for the production sweep worker. Built at the spawn site
/// (`run_server`) from `AppState`; kept as a plain struct so this module stays in
/// the library crate (it must not reference the binary-only `AppState`).
pub struct SweepWorker {
    pub s3: crate::s3_client::S3Client,
    pub storj: crate::storj_s3_client::StorjS3Client,
    pub db_url: String,
    pub drain_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub import_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The media-job cancellation token holder. Re-read each pass so the
    /// `media-cancel` endpoint can interrupt an in-flight worker drain.
    pub media_cancel: std::sync::Arc<std::sync::Mutex<tokio_util::sync::CancellationToken>>,
    pub me: String,
    pub drain_interval: std::time::Duration,
    pub discovery_interval: std::time::Duration,
    pub lease_ttl: std::time::Duration,
    pub failed_within: std::time::Duration,
}

impl SweepWorker {
    /// Run the resilient leased loop until `shutdown` (the graceful-shutdown token)
    /// fires. Each pass connects its own DB client; errors AND panics in a pass are
    /// logged + reported and the loop continues — the worker never exits on its own.
    pub async fn run(self, shutdown: tokio_util::sync::CancellationToken) {
        use futures::future::FutureExt;
        tracing::info!(me = %self.me, "sweep worker: started");
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                // catch_unwind so a panic in a pass (e.g. an unwrap in a dependency)
                // is contained and the loop survives — without it a panic would unwind
                // and silently kill this fire-and-forget task (only signal: stale heartbeat).
                res = std::panic::AssertUnwindSafe(self.run_pass()).catch_unwind() => {
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "sweep worker: pass error");
                            sentry::capture_message(
                                &format!("sweep worker pass error: {e}"),
                                sentry::Level::Warning,
                            );
                        }
                        Err(_panic) => {
                            tracing::error!("sweep worker: pass PANICKED (loop continues)");
                            sentry::capture_message(
                                "sweep worker pass panicked",
                                sentry::Level::Error,
                            );
                        }
                    }
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(self.drain_interval) => {}
            }
        }
        // Best-effort release so a peer takes over immediately (no TTL wait).
        if let Ok(client) = crate::db::connect(&self.db_url).await {
            let _ = crate::media_index::release_lease(&client, &self.me).await;
        }
        tracing::info!(me = %self.me, "sweep worker: stopped");
    }

    async fn run_pass(&self) -> Result<(), tokio_postgres::Error> {
        let client = std::sync::Arc::new(crate::db::connect(&self.db_url).await?);

        // Current media-job cancel token — so `media-cancel` can halt the worker drain.
        let media_cancel = self
            .media_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let drain = {
            let s3 = self.s3.clone();
            let storj = self.storj.clone();
            let db_url = self.db_url.clone();
            let cancel = media_cancel.clone();
            move || async move {
                if let Err(e) = crate::jobs::media_phash::run(
                    s3,
                    storj,
                    db_url,
                    cancel,
                    None,
                    "sweep_drain",
                    None,
                )
                .await
                {
                    tracing::warn!(error = %e, "sweep drain: media_phash failed");
                }
            }
        };

        let discovery = {
            let s3 = self.s3.clone();
            let storj = self.storj.clone();
            let db_url = self.db_url.clone();
            let cancel = media_cancel.clone();
            move || async move {
                // Full-scan both buckets (UUID keys are non-monotonic, so incremental
                // scan is lossy), then import the newly discovered rows into the master.
                if let Err(e) = crate::jobs::scan_hetzner::run(
                    s3,
                    db_url.clone(),
                    cancel.clone(),
                    None,
                    None,
                    true,
                )
                .await
                {
                    tracing::warn!(error = %e, "sweep discovery: scan_hetzner failed");
                }
                if let Err(e) = crate::jobs::scan_storj::run(
                    storj,
                    db_url.clone(),
                    cancel.clone(),
                    None,
                    None,
                    true,
                )
                .await
                {
                    tracing::warn!(error = %e, "sweep discovery: scan_storj failed");
                }
                match crate::db::connect(&db_url).await {
                    Ok(mut c) => {
                        if let Err(e) = crate::jobs::media_imports::import_current_video_index(
                            &mut c,
                            "sweep_discovery",
                            None,
                            &cancel,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "sweep discovery: import failed");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "sweep discovery: db connect failed"),
                }
            }
        };

        run_one_pass(
            &self.me,
            client,
            &self.drain_flag,
            &self.import_flag,
            self.lease_ttl,
            self.discovery_interval,
            self.failed_within,
            drain,
            discovery,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_index::{acquire_or_renew_lease, test_support::test_client};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn discovery_due_logic() {
        let now = chrono::Utc::now();
        assert!(discovery_due(None, Duration::from_secs(86400), now)); // never run
        assert!(discovery_due(
            Some(now - chrono::Duration::hours(25)),
            Duration::from_secs(86400),
            now
        ));
        assert!(!discovery_due(
            Some(now - chrono::Duration::hours(1)),
            Duration::from_secs(86400),
            now
        ));
    }

    #[tokio::test]
    async fn cas_guarded_skips_when_held() {
        let flag = Arc::new(AtomicBool::new(false));
        // held -> None, closure not run
        flag.store(true, Ordering::Release);
        let ran = Arc::new(AtomicBool::new(false));
        let r = cas_guarded(&flag, {
            let ran = ran.clone();
            move || async move {
                ran.store(true, Ordering::Release);
                1
            }
        })
        .await;
        assert_eq!(r, None);
        assert!(
            !ran.load(Ordering::Acquire),
            "closure must not run when held"
        );
        // free -> Some, flag reset after
        flag.store(false, Ordering::Release);
        let r2 = cas_guarded(&flag, || async { 42 }).await;
        assert_eq!(r2, Some(42));
        assert!(!flag.load(Ordering::Acquire), "flag reset on drop");
    }

    #[tokio::test]
    async fn heartbeat_renew_keeps_lease_fresh_during_long_task() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        let client = Arc::new(client);
        acquire_or_renew_lease(&client, "box-a", Duration::from_secs(2))
            .await
            .unwrap();

        // ttl=2s, renew every ~0.66s; a 3s task must keep the lease fresh so a peer can't steal.
        with_heartbeat_renew(
            client.clone(),
            "box-a".to_string(),
            Duration::from_secs(2),
            async {
                tokio::time::sleep(Duration::from_secs(3)).await;
            },
        )
        .await;

        assert!(
            !acquire_or_renew_lease(&client, "box-b", Duration::from_secs(2))
                .await
                .unwrap(),
            "peer cannot steal — heartbeat kept fresh"
        );
    }

    #[tokio::test]
    async fn pass_skips_everything_when_not_lease_owner() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        let client = Arc::new(client);
        // Another box owns a fresh lease.
        acquire_or_renew_lease(&client, "other-box", Duration::from_secs(60))
            .await
            .unwrap();

        let drained = Arc::new(AtomicBool::new(false));
        let dflag = Arc::new(AtomicBool::new(false));
        let iflag = Arc::new(AtomicBool::new(false));
        run_one_pass(
            "me",
            client.clone(),
            &dflag,
            &iflag,
            Duration::from_secs(60),
            Duration::from_secs(86400),
            Duration::from_secs(86400),
            {
                let d = drained.clone();
                move || async move { d.store(true, Ordering::Release) }
            },
            || async {},
        )
        .await
        .unwrap();

        assert!(!drained.load(Ordering::Acquire), "non-owner must not drain");
    }

    #[tokio::test]
    async fn pass_skips_drain_when_nothing_eligible() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        let client = Arc::new(client);
        // We own the lease, but there are no servable rows -> nothing eligible.
        let drained = Arc::new(AtomicBool::new(false));
        let dflag = Arc::new(AtomicBool::new(false));
        let iflag = Arc::new(AtomicBool::new(false));
        run_one_pass(
            "me",
            client.clone(),
            &dflag,
            &iflag,
            Duration::from_secs(60),
            // discovery_interval huge so discovery never fires in this focused test
            Duration::from_secs(u32::MAX as u64),
            Duration::from_secs(86400),
            {
                let d = drained.clone();
                move || async move { d.store(true, Ordering::Release) }
            },
            || async {},
        )
        .await
        .unwrap();

        assert!(
            !drained.load(Ordering::Acquire),
            "no eligible rows -> drain must not run"
        );
    }
}

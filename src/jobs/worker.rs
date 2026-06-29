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
    let out = fut.await;
    renew.abort();
    out
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
}

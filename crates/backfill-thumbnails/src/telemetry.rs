pub fn init_sentry() -> sentry::ClientInitGuard {
    sentry::init((
        "https://c861bc2e5d9555e56ab5e7e0619938c2@sentry.prakash.yral.com/6",
        sentry::ClientOptions {
            release: sentry::release_name!(),
            environment: Some(
                std::env::var("ENVIRONMENT")
                    .unwrap_or_else(|_| "production".to_string())
                    .into(),
            ),
            attach_stacktrace: true,
            ..Default::default()
        },
    ))
}

/// Ignore SIGPIPE so the binary survives a broken pipe (e.g. when the bash shell in the
/// GitHub Actions pipeline exits) long enough to flush Sentry before we exit ourselves.
pub fn ignore_sigpipe() {
    // SAFETY: zeroed sigaction is valid; SIG_IGN is a defined disposition; sa_mask and
    // sa_flags are explicitly initialized. sigaction is preferred over signal() for
    // consistent semantics across platforms. Process-wide effect is intentional — we
    // want write errors (EPIPE) instead of abrupt termination so SIGHUP/SIGTERM handlers
    // can flush Sentry before exit.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGPIPE, &sa, std::ptr::null_mut());
    }
}

// Signal number written by the raw C handler; 0 means no signal yet.
static KILL_SIGNAL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

extern "C" fn handle_kill_signal(sig: libc::c_int) {
    // SAFETY: store is async-signal-safe (single atomic write).
    KILL_SIGNAL.store(sig as u8, std::sync::atomic::Ordering::SeqCst);
}

/// Install raw sigaction handlers for SIGTERM and SIGHUP.
///
/// Must be called before `spawn_sigterm_flush`. Using raw sigaction instead of
/// Tokio's async signal stream means the handler fires even when the Tokio runtime
/// is saturated or its I/O driver is backlogged (e.g. flood of SIGCHLD from child
/// processes). SA_RESETHAND restores the default disposition after first delivery so
/// a second signal kills the process immediately if our flush takes too long.
pub fn install_kill_signal_handlers() {
    // SAFETY: zeroed sigaction is valid; handle_kill_signal is a valid fn ptr;
    // sa_mask and sa_flags are explicitly initialized; SA_RESETHAND is safe here.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handle_kill_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESETHAND;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
    }
}

/// Spawns a dedicated OS thread (independent of the Tokio runtime) that polls the
/// signal atomic set by `install_kill_signal_handlers`. On signal: logs to Sentry,
/// flushes, then exits 143.
pub fn spawn_sigterm_flush() {
    std::thread::spawn(|| loop {
        let sig = KILL_SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
        if sig != 0 {
            let name = match sig as libc::c_int {
                libc::SIGTERM => "SIGTERM",
                libc::SIGHUP => "SIGHUP",
                _ => "signal",
            };
            tracing::error!("process killed by runner ({name}) — flushing Sentry before exit");
            flush_sentry();
            std::process::exit(143);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    });
}

pub fn flush_sentry() {
    if let Some(client) = sentry::Hub::current().client() {
        client.flush(Some(std::time::Duration::from_secs(5)));
    }
}

pub fn init_tracing() {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::Layer;

    let sentry_layer = sentry_tracing::layer().event_filter(|metadata| match *metadata.level() {
        tracing::Level::ERROR => sentry_tracing::EventFilter::Event,
        tracing::Level::WARN => sentry_tracing::EventFilter::Breadcrumb,
        _ => sentry_tracing::EventFilter::Ignore,
    });

    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        ))
        .with(sentry_layer)
        .try_init();
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires SENTRY_DSN env var and live Sentry connection — run manually to verify"]
    fn sentry_receives_test_event() {
        let dsn = std::env::var("SENTRY_DSN").expect("SENTRY_DSN must be set");
        let _guard = sentry::init((
            dsn.as_str(),
            sentry::ClientOptions {
                release: sentry::release_name!(),
                ..Default::default()
            },
        ));
        sentry::capture_message(
            "backfill-thumbnails: sentry integration test event",
            sentry::Level::Error,
        );
        // guard drop flushes all pending events before the test exits
    }
}

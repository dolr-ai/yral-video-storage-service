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

/// Spawns a background task that listens for SIGTERM/SIGHUP and flushes Sentry before
/// exiting. GitHub Actions kills the bash step with SIGTERM; the binary in the pipeline
/// then gets SIGHUP when the shell exits. We handle both so Sentry always gets the event.
///
/// This uses Tokio's async signal stream rather than raw sigaction. The key prerequisite
/// is that the Tokio runtime must not be overwhelmed (e.g. by a SIGCHLD flood from
/// child processes with kill_on_drop). As long as that is avoided, this approach is
/// reliable and the signal task is polled promptly on signal delivery.
pub fn spawn_sigterm_flush() {
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!("failed to register SIGTERM handler: {err}");
                return;
            }
        };
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!("failed to register SIGHUP handler: {err}");
                return;
            }
        };

        let sig = tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sighup.recv()  => "SIGHUP",
        };
        tracing::error!("process killed by runner ({sig}) — flushing Sentry before exit");
        flush_sentry();
        std::process::exit(143);
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

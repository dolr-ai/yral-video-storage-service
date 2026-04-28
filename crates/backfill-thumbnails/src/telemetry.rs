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

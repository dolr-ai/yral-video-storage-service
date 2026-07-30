//! Versioned schema migrations (refinery).
//!
//! Replaces the three hand-rolled `SCHEMA_SQL` constants that were replayed on
//! every boot (`db.rs`, `media_index/schema.rs`, `videogen/request_store.rs`).
//! Those could not express an ordered or destructive change and left no record
//! of what a given database had actually seen. See the design spec,
//! § "Schema management — adopt versioned migrations".
//!
//! `V1__baseline.sql` is those three constants concatenated verbatim. Databases
//! that already ran them are *adopted* — V1 is stamped as applied without being
//! executed. Fresh databases run it normally.

use refinery::embed_migrations;

embed_migrations!("./migrations");

/// The baseline migration version. Only this version is ever faked during
/// adoption; see `run_migrations`.
const BASELINE_VERSION: i32 = 1;

/// Apply all pending migrations, adopting a pre-refinery database if needed.
pub async fn run_migrations(client: &mut tokio_postgres::Client) -> anyhow::Result<()> {
    if needs_baseline_adoption(client).await? {
        tracing::info!("pre-refinery schema detected; stamping V{BASELINE_VERSION} as applied");
        // FakeVersion(1), NOT Fake: stamp ONLY the baseline, never whatever else
        // happens to be pending. `Fake` would mark every pending migration as
        // applied without running it, so a database adopted after V2 existed
        // would silently skip V2's tables entirely.
        migrations::runner()
            .set_target(refinery::Target::FakeVersion(BASELINE_VERSION))
            .run_async(client)
            .await?;
    }

    // Runs whatever is still pending: V2 onward on an adopted database, V1
    // onward on a fresh one. Correct in either build order.
    let report = migrations::runner().run_async(client).await?;
    for m in report.applied_migrations() {
        tracing::info!(version = m.version(), name = m.name(), "migration applied");
    }
    Ok(())
}

/// True when the database carries the old hand-rolled schema but no refinery
/// history — i.e. this is the one-time adoption. `all_servable_videos_on_yral`
/// is the probe because only the pre-refinery `media_index` schema created it.
async fn needs_baseline_adoption(
    client: &tokio_postgres::Client,
) -> Result<bool, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT to_regclass('public.all_servable_videos_on_yral') IS NOT NULL
                AND to_regclass('public.refinery_schema_history') IS NULL AS adopt",
            &[],
        )
        .await?;
    Ok(row.get("adopt"))
}

#[cfg(test)]
mod tests {
    use crate::media_index::test_support::test_client;

    /// Every table the three pre-refinery constants used to create.
    const BASELINE_TABLES: &[&str] = &[
        "all_servable_videos_on_yral",
        "media_feed_events",
        "media_job_failures",
        "media_job_runs",
        "mirror_jobs",
        "servable_video_hashes",
        "servable_video_sources",
        "sweep_lease",
        "video_index",
        "videogen_requests",
        "yral_posts",
        "yral_users",
    ];

    async fn present_tables(client: &tokio_postgres::Client, want: &[&str]) -> Vec<String> {
        let rows = client
            .query(
                "SELECT table_name FROM information_schema.tables
                 WHERE table_schema = 'public' AND table_name = ANY($1)
                 ORDER BY table_name",
                &[&want.iter().map(|s| s.to_string()).collect::<Vec<_>>()],
            )
            .await
            .unwrap();
        rows.into_iter().map(|r| r.get(0)).collect()
    }

    async fn applied_versions(client: &tokio_postgres::Client) -> Vec<i32> {
        let rows = client
            .query(
                "SELECT version FROM refinery_schema_history ORDER BY version",
                &[],
            )
            .await
            .unwrap();
        rows.into_iter().map(|r| r.get(0)).collect()
    }

    #[tokio::test]
    async fn fresh_database_gets_the_full_baseline() {
        let (_pg, mut c) = test_client().await;
        super::run_migrations(&mut c).await.unwrap();

        assert_eq!(
            present_tables(&c, BASELINE_TABLES).await,
            BASELINE_TABLES,
            "V1 must create every table the old init_schema pair created"
        );
        assert_eq!(applied_versions(&c).await, vec![1]);
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let (_pg, mut c) = test_client().await;
        super::run_migrations(&mut c).await.unwrap();
        super::run_migrations(&mut c).await.unwrap();
        assert_eq!(applied_versions(&c).await, vec![1]);
    }

    /// The production path: a database that already ran the old constants must
    /// have V1 stamped, not re-executed, and must not error.
    #[tokio::test]
    async fn pre_refinery_database_is_adopted_not_rerun() {
        let (_pg, mut c) = test_client().await;
        crate::db::init_schema(&c).await.unwrap();
        crate::media_index::init_schema(&c).await.unwrap();
        crate::videogen::request_store::init_schema(&c)
            .await
            .unwrap();

        assert!(
            super::needs_baseline_adoption(&c).await.unwrap(),
            "old schema present + no history => adoption path"
        );

        super::run_migrations(&mut c).await.unwrap();

        assert_eq!(applied_versions(&c).await, vec![1]);
        assert!(
            !super::needs_baseline_adoption(&c).await.unwrap(),
            "adoption must be a one-time transition"
        );
        // Adoption must not have destroyed anything.
        assert_eq!(present_tables(&c, BASELINE_TABLES).await, BASELINE_TABLES);
    }
}

use std::str::FromStr;

use tokio_postgres::{Client, Transaction};

use super::{FeedEvent, FeedEventInput, FeedEventKind};

const MEDIA_FEED_EVENT_APPEND_LOCK: i64 = 904_648_332_137_142_901;
const MAX_FEED_READ_LIMIT: i64 = 500;

#[derive(Debug, thiserror::Error)]
pub enum FeedReadError {
    #[error("postgres feed read failed: {0}")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("unknown media feed event kind: {0}")]
    UnknownEventKind(String),
    #[error("invalid media feed read limit {limit}; max is {max}")]
    InvalidLimit { limit: i64, max: i64 },
}

pub async fn append_feed_event_txn(
    tx: &Transaction<'_>,
    input: FeedEventInput<'_>,
) -> Result<i64, tokio_postgres::Error> {
    // BIGSERIAL is allocation-ordered, not commit-ordered. The advisory lock keeps
    // feed-visible event insertion serialized so cursor paging cannot skip a
    // late-committing lower cursor.
    tx.execute(
        "SELECT pg_advisory_xact_lock($1)",
        &[&MEDIA_FEED_EVENT_APPEND_LOCK],
    )
    .await?;
    tx.batch_execute("SET LOCAL media_index.feed_event_append_locked = 'on'")
        .await?;

    let row = tx
        .query_one(
            "INSERT INTO media_feed_events (
                event_kind,
                video_id,
                hash_kind,
                hash_version,
                input_media_version,
                payload
             )
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING cursor",
            &[
                &input.event_kind.as_str(),
                &input.video_id,
                &input.hash_kind,
                &input.hash_version,
                &input.input_media_version,
                &input.payload,
            ],
        )
        .await?;

    Ok(row.get(0))
}

pub async fn list_feed_events_after(
    client: &Client,
    after_cursor: i64,
    limit: i64,
) -> Result<Vec<FeedEvent>, FeedReadError> {
    if !(1..=MAX_FEED_READ_LIMIT).contains(&limit) {
        return Err(FeedReadError::InvalidLimit {
            limit,
            max: MAX_FEED_READ_LIMIT,
        });
    }

    let rows = client
        .query(
            "SELECT cursor,
                    event_kind,
                    video_id,
                    hash_kind,
                    hash_version,
                    input_media_version,
                    payload,
                    created_at
             FROM media_feed_events
             WHERE cursor > $1
             ORDER BY cursor ASC
             LIMIT $2",
            &[&after_cursor, &limit],
        )
        .await?;

    rows.into_iter()
        .map(|row| {
            let event_kind: String = row.get(1);
            let event_kind = FeedEventKind::from_str(event_kind.as_str())
                .map_err(FeedReadError::UnknownEventKind)?;

            Ok(FeedEvent {
                cursor: row.get(0),
                event_kind,
                video_id: row.get(2),
                hash_kind: row.get(3),
                hash_version: row.get(4),
                input_media_version: row.get(5),
                payload: row.get(6),
                created_at: row.get(7),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::time::{timeout, Duration};

    use crate::media_index::test_support::{connect_test_client, test_client, PgContainer};

    async fn insert_video(client: &tokio_postgres::Client, video_id: &str) {
        crate::media_index::upsert_servable_video(
            client,
            crate::media_index::ServableVideoInput {
                video_id,
                publisher_user_id: Some("publisher-a"),
                post_id: Some("post-a"),
                source_kind: "legacy_video_index",
                source_ref: Some("legacy-ref"),
                servable_status: "servable",
                nsfw_state: Some("unknown"),
                storage_provider: Some("storj"),
                bucket: Some("videos"),
                object_key: Some("publisher-a/video.mp4"),
                canonical_url: Some("https://example.test/video.mp4"),
                thumbnail_key: Some("publisher-a/thumb.jpg"),
                duration_ms: Some(1000),
                width: Some(1280),
                height: Some(720),
                fps: Some(30.0),
                container: Some("mp4"),
                video_codec: Some("h264"),
                audio_codec: Some("aac"),
                moov_atom_front: Some(true),
                canonical_encoding_version: Some("source"),
                discovered_from: "test",
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn append_feed_event_txn_requires_a_transaction() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        insert_video(&client, "video-a").await;

        let err = client
            .execute(
                "INSERT INTO media_feed_events (event_kind, video_id, payload)
                 VALUES ('hash_upserted', 'video-a', '{}'::jsonb)",
                &[],
            )
            .await
            .unwrap_err();

        assert_eq!(
            err.code(),
            Some(&tokio_postgres::error::SqlState::RAISE_EXCEPTION)
        );
    }

    #[tokio::test]
    async fn feed_event_append_serializes_cursor_visible_writes() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        insert_video(&client, "video-a").await;
        insert_video(&client, "video-b").await;

        let tx = client.transaction().await.unwrap();
        let first_cursor = crate::media_index::append_feed_event_txn(
            &tx,
            crate::media_index::FeedEventInput {
                event_kind: crate::media_index::FeedEventKind::HashUpserted,
                video_id: "video-a",
                hash_kind: Some("phash"),
                hash_version: Some("offchain_binary_10x8_v1"),
                input_media_version: Some("source_v1"),
                payload: json!({
                    "video_id": "video-a",
                    "hash_kind": "phash",
                    "hash_version": "offchain_binary_10x8_v1",
                    "hash_value": "0101"
                }),
            },
        )
        .await
        .unwrap();
        let second_cursor = crate::media_index::append_feed_event_txn(
            &tx,
            crate::media_index::FeedEventInput {
                event_kind: crate::media_index::FeedEventKind::HashUpserted,
                video_id: "video-b",
                hash_kind: Some("phash"),
                hash_version: Some("offchain_binary_10x8_v1"),
                input_media_version: Some("source_v1"),
                payload: json!({
                    "video_id": "video-b",
                    "hash_kind": "phash",
                    "hash_version": "offchain_binary_10x8_v1",
                    "hash_value": "0101"
                }),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        assert!(first_cursor > 0);
        assert!(second_cursor > first_cursor);
    }

    #[tokio::test]
    async fn feed_payload_is_denormalized_and_read_without_source_table_join() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        insert_video(&client, "video-a").await;

        let tx = client.transaction().await.unwrap();
        let cursor = crate::media_index::append_feed_event_txn(
            &tx,
            crate::media_index::FeedEventInput {
                event_kind: crate::media_index::FeedEventKind::HashUpserted,
                video_id: "video-a",
                hash_kind: Some("phash"),
                hash_version: Some("offchain_binary_10x8_v1"),
                input_media_version: Some("source_v1"),
                payload: json!({
                    "video_id": "video-a",
                    "canonical_url": "https://example.test/video.mp4",
                    "object_key": "publisher-a/video.mp4",
                    "hash_kind": "phash",
                    "hash_version": "offchain_binary_10x8_v1",
                    "hash_value": "0101"
                }),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        client
            .execute("DROP TABLE servable_video_sources", &[])
            .await
            .unwrap();

        let events = crate::media_index::list_feed_events_after(&client, 0, 10)
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cursor, cursor);
        assert_eq!(
            events[0].payload["canonical_url"],
            "https://example.test/video.mp4"
        );
        assert_eq!(events[0].payload["object_key"], "publisher-a/video.mp4");
    }

    #[tokio::test]
    async fn unknown_feed_event_kind_is_not_decoded_as_hash_upserted() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        insert_video(&client, "video-a").await;

        let tx = client.transaction().await.unwrap();
        tx.batch_execute("SET LOCAL media_index.feed_event_append_locked = 'on'")
            .await
            .unwrap();
        tx.execute(
            "INSERT INTO media_feed_events (event_kind, video_id, payload)
             VALUES ('unexpected_event', 'video-a', '{}'::jsonb)",
            &[],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let err = crate::media_index::list_feed_events_after(&client, 0, 10)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            crate::media_index::FeedReadError::UnknownEventKind(kind)
                if kind == "unexpected_event"
        ));
    }

    #[tokio::test]
    async fn feed_read_rejects_unsafe_limits() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        let err = crate::media_index::list_feed_events_after(&client, 0, 0)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::media_index::FeedReadError::InvalidLimit { limit: 0, .. }
        ));

        let err = crate::media_index::list_feed_events_after(&client, 0, 501)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::media_index::FeedReadError::InvalidLimit {
                limit: 501,
                max: 500
            }
        ));
    }

    #[tokio::test]
    async fn advisory_lock_blocks_later_cursor_until_earlier_append_commits() {
        let (pg, url) = PgContainer::spawn().await;
        let mut first_client = connect_test_client(&url).await;
        let mut second_client = connect_test_client(&url).await;
        crate::media_index::init_schema(&first_client)
            .await
            .unwrap();
        insert_video(&first_client, "video-a").await;
        insert_video(&first_client, "video-b").await;

        let first_tx = first_client.transaction().await.unwrap();
        let first_cursor = crate::media_index::append_feed_event_txn(
            &first_tx,
            crate::media_index::FeedEventInput {
                event_kind: crate::media_index::FeedEventKind::HashUpserted,
                video_id: "video-a",
                hash_kind: Some("phash"),
                hash_version: Some("offchain_binary_10x8_v1"),
                input_media_version: Some("source_v1"),
                payload: json!({"video_id": "video-a"}),
            },
        )
        .await
        .unwrap();

        let mut second_append = tokio::spawn(async move {
            let second_tx = second_client.transaction().await.unwrap();
            let cursor = crate::media_index::append_feed_event_txn(
                &second_tx,
                crate::media_index::FeedEventInput {
                    event_kind: crate::media_index::FeedEventKind::HashUpserted,
                    video_id: "video-b",
                    hash_kind: Some("phash"),
                    hash_version: Some("offchain_binary_10x8_v1"),
                    input_media_version: Some("source_v1"),
                    payload: json!({"video_id": "video-b"}),
                },
            )
            .await
            .unwrap();
            second_tx.commit().await.unwrap();
            cursor
        });

        assert!(timeout(Duration::from_millis(150), &mut second_append)
            .await
            .is_err());
        first_tx.commit().await.unwrap();

        let second_cursor = second_append.await.unwrap();
        assert!(second_cursor > first_cursor);

        let events = crate::media_index::list_feed_events_after(&first_client, 0, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].cursor, first_cursor);
        assert_eq!(events[1].cursor, second_cursor);
        drop(pg);
    }
}

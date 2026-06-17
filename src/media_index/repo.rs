use serde_json::Value;
use tokio_postgres::{Client, GenericClient, Row, Transaction};

use super::{
    ExactDuplicateQuery, HashRecord, HashRecordInput, ServableVideoInput,
    ServableVideoUpsertOutcome, UpsertOutcome,
};

/// Test/setup convenience wrapper. Feed-visible production writes should use
/// `upsert_servable_video_txn` so source rows and feed events commit together.
#[cfg(test)]
pub async fn upsert_servable_video(
    client: &Client,
    input: ServableVideoInput<'_>,
) -> Result<ServableVideoUpsertOutcome, tokio_postgres::Error> {
    upsert_servable_video_with_client(client, input).await
}

pub async fn upsert_servable_video_txn(
    tx: &Transaction<'_>,
    input: ServableVideoInput<'_>,
) -> Result<ServableVideoUpsertOutcome, tokio_postgres::Error> {
    upsert_servable_video_with_client(tx, input).await
}

async fn upsert_servable_video_with_client(
    client: &(impl GenericClient + Sync),
    input: ServableVideoInput<'_>,
) -> Result<ServableVideoUpsertOutcome, tokio_postgres::Error> {
    let inserted = client
        .query_opt(
            "INSERT INTO all_servable_videos_on_yral (
                video_id,
                publisher_user_id,
                post_id,
                source_kind,
                source_ref,
                servable_status,
                nsfw_state,
                storage_provider,
                bucket,
                object_key,
                canonical_url,
                thumbnail_key,
                duration_ms,
                width,
                height,
                fps,
                container,
                video_codec,
                audio_codec,
                moov_atom_front,
                canonical_encoding_version,
                discovered_from
             )
             VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22
             )
             ON CONFLICT (video_id) DO NOTHING
             RETURNING video_id",
            &[
                &input.video_id,
                &input.publisher_user_id,
                &input.post_id,
                &input.source_kind,
                &input.source_ref,
                &input.servable_status,
                &input.nsfw_state,
                &input.storage_provider,
                &input.bucket,
                &input.object_key,
                &input.canonical_url,
                &input.thumbnail_key,
                &input.duration_ms,
                &input.width,
                &input.height,
                &input.fps,
                &input.container,
                &input.video_codec,
                &input.audio_codec,
                &input.moov_atom_front,
                &input.canonical_encoding_version,
                &input.discovered_from,
            ],
        )
        .await?;

    let source_inserted = insert_source_row(client, &input).await?;

    if inserted.is_some() {
        return Ok(ServableVideoUpsertOutcome {
            media: UpsertOutcome::Inserted,
            source_inserted,
        });
    }

    let existing = client
        .query_one(
            "SELECT publisher_user_id,
                    post_id,
                    source_kind,
                    source_ref,
                    servable_status,
                    nsfw_state,
                    storage_provider,
                    bucket,
                    object_key,
                    canonical_url,
                    thumbnail_key,
                    duration_ms,
                    width,
                    height,
                    fps,
                    container,
                    video_codec,
                    audio_codec,
                    moov_atom_front,
                    canonical_encoding_version,
                    discovered_from
             FROM all_servable_videos_on_yral
             WHERE video_id = $1
             FOR UPDATE",
            &[&input.video_id],
        )
        .await?;

    if servable_video_material_matches(&existing, &input) {
        return Ok(ServableVideoUpsertOutcome {
            media: UpsertOutcome::Unchanged,
            source_inserted,
        });
    }

    client
        .execute(
            "UPDATE all_servable_videos_on_yral
             SET publisher_user_id = COALESCE($2, publisher_user_id),
                 post_id = COALESCE($3, post_id),
                 source_kind = $4,
                 source_ref = COALESCE(source_ref, $5),
                 servable_status = $6,
                 nsfw_state = COALESCE($7, nsfw_state),
                 storage_provider = COALESCE($8, storage_provider),
                 bucket = COALESCE($9, bucket),
                 object_key = COALESCE($10, object_key),
                 canonical_url = COALESCE($11, canonical_url),
                 thumbnail_key = COALESCE($12, thumbnail_key),
                 duration_ms = COALESCE($13, duration_ms),
                 width = COALESCE($14, width),
                 height = COALESCE($15, height),
                 fps = COALESCE($16, fps),
                 container = COALESCE($17, container),
                 video_codec = COALESCE($18, video_codec),
                 audio_codec = COALESCE($19, audio_codec),
                 moov_atom_front = COALESCE($20, moov_atom_front),
                 canonical_encoding_version = COALESCE($21, canonical_encoding_version),
                 discovered_from = $22,
                 last_seen_at = NOW(),
                 updated_at = NOW()
             WHERE video_id = $1",
            &[
                &input.video_id,
                &input.publisher_user_id,
                &input.post_id,
                &input.source_kind,
                &input.source_ref,
                &input.servable_status,
                &input.nsfw_state,
                &input.storage_provider,
                &input.bucket,
                &input.object_key,
                &input.canonical_url,
                &input.thumbnail_key,
                &input.duration_ms,
                &input.width,
                &input.height,
                &input.fps,
                &input.container,
                &input.video_codec,
                &input.audio_codec,
                &input.moov_atom_front,
                &input.canonical_encoding_version,
                &input.discovered_from,
            ],
        )
        .await?;

    Ok(ServableVideoUpsertOutcome {
        media: UpsertOutcome::Changed,
        source_inserted,
    })
}

async fn insert_source_row(
    client: &(impl GenericClient + Sync),
    input: &ServableVideoInput<'_>,
) -> Result<bool, tokio_postgres::Error> {
    if let Some(source_ref) = input.source_ref {
        let inserted = client
            .query_opt(
                "INSERT INTO servable_video_sources (
                    video_id,
                    source_kind,
                    source_ref,
                    raw_payload
                 )
                 VALUES ($1, $2, $3, NULL)
                 ON CONFLICT (video_id, source_kind, source_ref) DO NOTHING
                 RETURNING id",
                &[&input.video_id, &input.source_kind, &source_ref],
            )
            .await?;
        return Ok(inserted.is_some());
    }

    Ok(false)
}

fn servable_video_material_matches(row: &Row, input: &ServableVideoInput<'_>) -> bool {
    optional_str_matches(row, 0, input.publisher_user_id)
        && optional_str_matches(row, 1, input.post_id)
        && row.get::<_, String>(2) == input.source_kind
        && source_ref_matches(row, 3, input.source_ref)
        && row.get::<_, String>(4) == input.servable_status
        && optional_str_matches(row, 5, input.nsfw_state)
        && optional_str_matches(row, 6, input.storage_provider)
        && optional_str_matches(row, 7, input.bucket)
        && optional_str_matches(row, 8, input.object_key)
        && optional_str_matches(row, 9, input.canonical_url)
        && optional_str_matches(row, 10, input.thumbnail_key)
        && optional_eq_matches(row, 11, input.duration_ms)
        && optional_eq_matches(row, 12, input.width)
        && optional_eq_matches(row, 13, input.height)
        && optional_eq_matches(row, 14, input.fps)
        && optional_str_matches(row, 15, input.container)
        && optional_str_matches(row, 16, input.video_codec)
        && optional_str_matches(row, 17, input.audio_codec)
        && optional_eq_matches(row, 18, input.moov_atom_front)
        && optional_str_matches(row, 19, input.canonical_encoding_version)
        && row.get::<_, String>(20) == input.discovered_from
}

fn optional_str_matches(row: &Row, idx: usize, incoming: Option<&str>) -> bool {
    incoming.is_none_or(|incoming| row.get::<_, Option<String>>(idx).as_deref() == Some(incoming))
}

fn source_ref_matches(row: &Row, idx: usize, incoming: Option<&str>) -> bool {
    let existing = row.get::<_, Option<String>>(idx);
    existing.is_some() || incoming.is_none()
}

fn optional_eq_matches<T>(row: &Row, idx: usize, incoming: Option<T>) -> bool
where
    T: PartialEq + tokio_postgres::types::FromSqlOwned,
{
    incoming.is_none_or(|incoming| row.get::<_, Option<T>>(idx) == Some(incoming))
}

pub async fn upsert_hash_record_txn(
    tx: &Transaction<'_>,
    input: HashRecordInput<'_>,
) -> Result<UpsertOutcome, tokio_postgres::Error> {
    let inserted = tx
        .query_opt(
            "INSERT INTO servable_video_hashes (
                video_id,
                hash_kind,
                hash_version,
                input_media_version,
                hash_value,
                hash_bit_length,
                num_frames,
                hash_size,
                computed_from_provider,
                computed_from_bucket,
                computed_from_key,
                 metadata
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (video_id, hash_kind, hash_version, input_media_version) DO NOTHING
             RETURNING computed_at",
            &[
                &input.video_id,
                &input.hash_kind,
                &input.hash_version,
                &input.input_media_version,
                &input.hash_value,
                &input.hash_bit_length,
                &input.num_frames,
                &input.hash_size,
                &input.computed_from_provider,
                &input.computed_from_bucket,
                &input.computed_from_key,
                &input.metadata,
            ],
        )
        .await?;

    if inserted.is_some() {
        return Ok(UpsertOutcome::Inserted);
    }

    let existing = tx
        .query_one(
            "SELECT hash_value,
                    hash_bit_length,
                    num_frames,
                    hash_size,
                    computed_from_provider,
                    computed_from_bucket,
                    computed_from_key,
                    metadata
             FROM servable_video_hashes
             WHERE video_id = $1
               AND hash_kind = $2
               AND hash_version = $3
               AND input_media_version = $4
             FOR UPDATE",
            &[
                &input.video_id,
                &input.hash_kind,
                &input.hash_version,
                &input.input_media_version,
            ],
        )
        .await?;

    if hash_material_matches(&existing, &input) {
        return Ok(UpsertOutcome::Unchanged);
    }

    tx.execute(
        "UPDATE servable_video_hashes
         SET hash_value = $5,
             hash_bit_length = $6,
             num_frames = $7,
             hash_size = $8,
             computed_from_provider = $9,
             computed_from_bucket = $10,
             computed_from_key = $11,
             computed_at = NOW(),
             metadata = $12
         WHERE video_id = $1
           AND hash_kind = $2
           AND hash_version = $3
           AND input_media_version = $4",
        &[
            &input.video_id,
            &input.hash_kind,
            &input.hash_version,
            &input.input_media_version,
            &input.hash_value,
            &input.hash_bit_length,
            &input.num_frames,
            &input.hash_size,
            &input.computed_from_provider,
            &input.computed_from_bucket,
            &input.computed_from_key,
            &input.metadata,
        ],
    )
    .await?;

    Ok(UpsertOutcome::Changed)
}

fn hash_material_matches(row: &Row, input: &HashRecordInput<'_>) -> bool {
    let metadata: Option<Value> = row.get(7);

    row.get::<_, String>(0) == input.hash_value
        && row.get::<_, i32>(1) == input.hash_bit_length
        && row.get::<_, i32>(2) == input.num_frames
        && row.get::<_, i32>(3) == input.hash_size
        && row.get::<_, Option<String>>(4).as_deref() == input.computed_from_provider
        && row.get::<_, Option<String>>(5).as_deref() == input.computed_from_bucket
        && row.get::<_, Option<String>>(6).as_deref() == input.computed_from_key
        && metadata.as_ref() == input.metadata.as_ref()
}

pub async fn find_exact_duplicates(
    client: &Client,
    query: ExactDuplicateQuery<'_>,
) -> Result<Vec<HashRecord>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT video_id,
                    hash_kind,
                    hash_version,
                    input_media_version,
                    hash_value,
                    hash_bit_length,
                    num_frames,
                    hash_size,
                    computed_from_provider,
                    computed_from_bucket,
                    computed_from_key,
                    computed_at,
                    metadata
             FROM servable_video_hashes
             WHERE hash_kind = $1
               AND hash_version = $2
               AND hash_value = $3
             ORDER BY video_id, input_media_version",
            &[&query.hash_kind, &query.hash_version, &query.hash_value],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| HashRecord {
            video_id: row.get(0),
            hash_kind: row.get(1),
            hash_version: row.get(2),
            input_media_version: row.get(3),
            hash_value: row.get(4),
            hash_bit_length: row.get(5),
            num_frames: row.get(6),
            hash_size: row.get(7),
            computed_from_provider: row.get(8),
            computed_from_bucket: row.get(9),
            computed_from_key: row.get(10),
            computed_at: row.get(11),
            metadata: row.get(12),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::media_index::test_support::test_client;

    fn video_input(video_id: &str) -> crate::media_index::ServableVideoInput<'_> {
        crate::media_index::ServableVideoInput {
            video_id,
            publisher_user_id: Some("publisher-a"),
            post_id: None,
            source_kind: "legacy_video_index",
            source_ref: Some(video_id),
            servable_status: "servable",
            nsfw_state: None,
            storage_provider: Some("storj"),
            bucket: Some("videos"),
            object_key: Some(video_id),
            canonical_url: None,
            thumbnail_key: None,
            duration_ms: None,
            width: None,
            height: None,
            fps: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            moov_atom_front: None,
            canonical_encoding_version: None,
            discovered_from: "test",
        }
    }

    fn hash_input<'a>(
        video_id: &'a str,
        input_media_version: &'a str,
    ) -> crate::media_index::HashRecordInput<'a> {
        crate::media_index::HashRecordInput {
            video_id,
            hash_kind: "phash",
            hash_version: "offchain_binary_10x8_v1",
            input_media_version,
            hash_value: "0101",
            hash_bit_length: 4,
            num_frames: 1,
            hash_size: 8,
            computed_from_provider: Some("storj"),
            computed_from_bucket: Some("videos"),
            computed_from_key: Some(video_id),
            metadata: Some(json!({"source": "test"})),
        }
    }

    #[tokio::test]
    async fn exact_duplicate_lookup_returns_matching_hash_rows_without_collapsing_primary_keys() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        let tx = client.transaction().await.unwrap();
        assert_eq!(
            crate::media_index::upsert_servable_video_txn(&tx, video_input("video-a"))
                .await
                .unwrap()
                .media,
            crate::media_index::UpsertOutcome::Inserted
        );
        assert_eq!(
            crate::media_index::upsert_servable_video_txn(&tx, video_input("video-b"))
                .await
                .unwrap()
                .media,
            crate::media_index::UpsertOutcome::Inserted
        );
        crate::media_index::upsert_hash_record_txn(&tx, hash_input("video-a", "source-v1"))
            .await
            .unwrap();
        crate::media_index::upsert_hash_record_txn(&tx, hash_input("video-b", "source-v1"))
            .await
            .unwrap();
        crate::media_index::upsert_hash_record_txn(&tx, hash_input("video-b", "source-v2"))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let duplicates = crate::media_index::find_exact_duplicates(
            &client,
            crate::media_index::ExactDuplicateQuery {
                hash_kind: "phash",
                hash_version: "offchain_binary_10x8_v1",
                hash_value: "0101",
            },
        )
        .await
        .unwrap();

        assert_eq!(duplicates.len(), 3);
        assert!(duplicates
            .iter()
            .any(|row| row.video_id == "video-a" && row.input_media_version == "source-v1"));
        assert!(duplicates
            .iter()
            .any(|row| row.video_id == "video-b" && row.input_media_version == "source-v1"));
        assert!(duplicates
            .iter()
            .any(|row| row.video_id == "video-b" && row.input_media_version == "source-v2"));
    }

    #[tokio::test]
    async fn servable_video_upsert_returns_outcome_and_leaves_timestamps_unchanged_for_identical_input(
    ) {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        let tx = client.transaction().await.unwrap();
        let outcome = crate::media_index::upsert_servable_video_txn(&tx, video_input("video-a"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(outcome.media, crate::media_index::UpsertOutcome::Inserted);

        let first_timestamps = client
            .query_one(
                "SELECT last_seen_at, updated_at
                 FROM all_servable_videos_on_yral
                 WHERE video_id = 'video-a'",
                &[],
            )
            .await
            .unwrap();
        let first_last_seen_at: chrono::DateTime<chrono::Utc> = first_timestamps.get(0);
        let first_updated_at: chrono::DateTime<chrono::Utc> = first_timestamps.get(1);

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let tx = client.transaction().await.unwrap();
        let outcome = crate::media_index::upsert_servable_video_txn(&tx, video_input("video-a"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(outcome.media, crate::media_index::UpsertOutcome::Unchanged);

        let unchanged_timestamps = client
            .query_one(
                "SELECT last_seen_at, updated_at
                 FROM all_servable_videos_on_yral
                 WHERE video_id = 'video-a'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            unchanged_timestamps.get::<_, chrono::DateTime<chrono::Utc>>(0),
            first_last_seen_at
        );
        assert_eq!(
            unchanged_timestamps.get::<_, chrono::DateTime<chrono::Utc>>(1),
            first_updated_at
        );

        let mut changed = video_input("video-a");
        changed.servable_status = "hidden";
        let tx = client.transaction().await.unwrap();
        let outcome = crate::media_index::upsert_servable_video_txn(&tx, changed)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(outcome.media, crate::media_index::UpsertOutcome::Changed);
    }

    #[tokio::test]
    async fn servable_video_upsert_reports_source_insert_even_when_media_row_is_unchanged() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        let mut original = video_input("video-a");
        original.source_ref = Some("source-a");
        let tx = client.transaction().await.unwrap();
        let outcome = crate::media_index::upsert_servable_video_txn(&tx, original)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(outcome.media, crate::media_index::UpsertOutcome::Inserted);
        assert!(outcome.source_inserted);

        let mut same_media_new_source = video_input("video-a");
        same_media_new_source.source_ref = Some("source-b");
        let tx = client.transaction().await.unwrap();
        let outcome = crate::media_index::upsert_servable_video_txn(&tx, same_media_new_source)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(outcome.media, crate::media_index::UpsertOutcome::Unchanged);
        assert!(outcome.source_inserted);
    }

    #[tokio::test]
    async fn hash_upsert_returns_outcome_and_leaves_computed_at_unchanged_for_identical_input() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        let tx = client.transaction().await.unwrap();
        crate::media_index::upsert_servable_video_txn(&tx, video_input("video-a"))
            .await
            .unwrap();
        let outcome =
            crate::media_index::upsert_hash_record_txn(&tx, hash_input("video-a", "source-v1"))
                .await
                .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(outcome, crate::media_index::UpsertOutcome::Inserted);

        let first_computed_at: chrono::DateTime<chrono::Utc> = client
            .query_one(
                "SELECT computed_at FROM servable_video_hashes WHERE video_id = 'video-a'",
                &[],
            )
            .await
            .unwrap()
            .get(0);

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let tx = client.transaction().await.unwrap();
        let outcome =
            crate::media_index::upsert_hash_record_txn(&tx, hash_input("video-a", "source-v1"))
                .await
                .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(outcome, crate::media_index::UpsertOutcome::Unchanged);

        let unchanged_computed_at: chrono::DateTime<chrono::Utc> = client
            .query_one(
                "SELECT computed_at FROM servable_video_hashes WHERE video_id = 'video-a'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(unchanged_computed_at, first_computed_at);

        let mut changed_hash = hash_input("video-a", "source-v1");
        changed_hash.hash_value = "1111";
        let tx = client.transaction().await.unwrap();
        let outcome = crate::media_index::upsert_hash_record_txn(&tx, changed_hash)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(outcome, crate::media_index::UpsertOutcome::Changed);
    }

    #[tokio::test]
    async fn sparse_media_upsert_preserves_existing_non_null_canonical_fields() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        let mut full = video_input("video-a");
        full.publisher_user_id = Some("publisher-a");
        full.post_id = Some("post-a");
        full.nsfw_state = Some("safe");
        full.canonical_url = Some("https://example.test/video-a.mp4");
        full.thumbnail_key = Some("thumbs/video-a.jpg");
        full.duration_ms = Some(1000);
        full.width = Some(1920);
        full.height = Some(1080);

        let tx = client.transaction().await.unwrap();
        assert_eq!(
            crate::media_index::upsert_servable_video_txn(&tx, full)
                .await
                .unwrap()
                .media,
            crate::media_index::UpsertOutcome::Inserted
        );
        tx.commit().await.unwrap();

        let sparse = crate::media_index::ServableVideoInput {
            video_id: "video-a",
            publisher_user_id: None,
            post_id: None,
            source_kind: "legacy_video_index",
            source_ref: Some("video-a"),
            servable_status: "servable",
            nsfw_state: None,
            storage_provider: None,
            bucket: None,
            object_key: None,
            canonical_url: None,
            thumbnail_key: None,
            duration_ms: None,
            width: None,
            height: None,
            fps: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            moov_atom_front: None,
            canonical_encoding_version: None,
            discovered_from: "sparse-import",
        };
        let tx = client.transaction().await.unwrap();
        assert_eq!(
            crate::media_index::upsert_servable_video_txn(&tx, sparse)
                .await
                .unwrap()
                .media,
            crate::media_index::UpsertOutcome::Changed
        );
        tx.commit().await.unwrap();

        let row = client
            .query_one(
                "SELECT publisher_user_id,
                        post_id,
                        nsfw_state,
                        canonical_url,
                        thumbnail_key,
                        duration_ms,
                        width,
                        height,
                        discovered_from
                 FROM all_servable_videos_on_yral
                 WHERE video_id = 'video-a'",
                &[],
            )
            .await
            .unwrap();

        assert_eq!(row.get::<_, String>(0), "publisher-a");
        assert_eq!(row.get::<_, String>(1), "post-a");
        assert_eq!(row.get::<_, String>(2), "safe");
        assert_eq!(row.get::<_, String>(3), "https://example.test/video-a.mp4");
        assert_eq!(row.get::<_, String>(4), "thumbs/video-a.jpg");
        assert_eq!(row.get::<_, i64>(5), 1000);
        assert_eq!(row.get::<_, i32>(6), 1920);
        assert_eq!(row.get::<_, i32>(7), 1080);
        assert_eq!(row.get::<_, String>(8), "sparse-import");
    }
}

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

/// A master-table row that is missing the canonical pHash tuple
/// `(hash_kind='phash', hash_version='offchain_binary_10x8_v1',
///   input_media_version='current_stored_object_v1')`.
///
/// Used by the `media_phash` job to discover work that still needs doing.
#[derive(Debug, Clone)]
pub struct MissingHashRow {
    pub video_id: String,
    pub storage_provider: Option<String>,
    pub object_key: Option<String>,
    pub servable_status: String,
    pub bucket: Option<String>,
}

/// Scan `all_servable_videos_on_yral` for rows that are MISSING the canonical
/// pHash tuple.  The query is incremental/resumable — rows already hashed are
/// simply absent from the result.
///
/// `after` is an exclusive `video_id` cursor: only rows with `video_id > after`
/// are returned (results are ordered by `video_id`). Callers paging a backfill
/// MUST advance this cursor past the last row of each batch; otherwise a row
/// that never gets a hash (e.g. a permanent compute failure) stays "missing"
/// and would be re-fetched forever.
pub async fn videos_missing_canonical_phash(
    client: &Client,
    hash_kind: &str,
    hash_version: &str,
    input_media_version: &str,
    after: Option<&str>,
    limit: Option<i64>,
    shard: Option<(i64, i64)>,
) -> Result<Vec<MissingHashRow>, tokio_postgres::Error> {
    let (shard_of, shard_idx): (Option<i64>, Option<i64>) = match shard {
        Some((of, idx)) => (Some(of), Some(idx)),
        None => (None, None),
    };
    let base = "SELECT v.video_id, v.storage_provider, v.object_key, v.servable_status, v.bucket
                FROM all_servable_videos_on_yral v
                LEFT JOIN servable_video_hashes h
                   ON h.video_id = v.video_id
                  AND h.hash_kind = $1 AND h.hash_version = $2 AND h.input_media_version = $3
                WHERE h.video_id IS NULL
                  AND ($4::TEXT IS NULL OR v.video_id > $4)
                  AND ($5::BIGINT IS NULL
                       OR (((hashtext(v.video_id)::bigint % $5) + $5) % $5) = $6)
                ORDER BY v.video_id";
    let rows = if let Some(limit) = limit {
        client
            .query(
                &format!("{base} LIMIT $7"),
                &[
                    &hash_kind,
                    &hash_version,
                    &input_media_version,
                    &after,
                    &shard_of,
                    &shard_idx,
                    &limit,
                ],
            )
            .await?
    } else {
        client
            .query(
                base,
                &[
                    &hash_kind,
                    &hash_version,
                    &input_media_version,
                    &after,
                    &shard_of,
                    &shard_idx,
                ],
            )
            .await?
    };
    Ok(rows
        .into_iter()
        .map(|row| MissingHashRow {
            video_id: row.get(0),
            storage_provider: row.get(1),
            object_key: row.get(2),
            servable_status: row.get(3),
            bucket: row.get(4),
        })
        .collect())
}

/// Coverage stats for the canonical pHash tuple
/// `(hash_kind='phash', hash_version='offchain_binary_10x8_v1',
///   input_media_version='current_stored_object_v1')`.
///
/// `total_servable` — total rows in `all_servable_videos_on_yral`.
/// `with_canonical_phash` — rows that have the canonical hash tuple.
/// `missing_canonical_phash` — rows that are still missing it.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageStats {
    pub total_servable: i64,
    pub with_canonical_phash: i64,
    pub missing_canonical_phash: i64,
}

pub async fn canonical_phash_coverage(
    client: &Client,
) -> Result<CoverageStats, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT
                COUNT(*) AS total_servable,
                COUNT(h.video_id) AS with_canonical_phash,
                COUNT(*) - COUNT(h.video_id) AS missing_canonical_phash
             FROM all_servable_videos_on_yral v
             LEFT JOIN servable_video_hashes h
                ON h.video_id = v.video_id
               AND h.hash_kind = 'phash'
               AND h.hash_version = 'offchain_binary_10x8_v1'
               AND h.input_media_version = 'current_stored_object_v1'",
            &[],
        )
        .await?;

    Ok(CoverageStats {
        total_servable: row.get(0),
        with_canonical_phash: row.get(1),
        missing_canonical_phash: row.get(2),
    })
}

/// Returns true iff there is at least one servable video that is MISSING a canonical
/// pHash AND was NOT failed within the last `failed_within` window. The recently-failed
/// anti-join quarantines permanently-dead rows (deleted source / never-finalizing temp
/// key) so the steady-state worker does not re-download them every tick (and does not
/// insert an empty `media_job_runs` row each pass). Short-circuits via `EXISTS … LIMIT 1`,
/// so it is far cheaper than a full `COUNT(*)` over the whole servable set.
pub async fn any_eligible_for_hash(
    client: &Client,
    hash_kind: &str,
    hash_version: &str,
    input_media_version: &str,
    failed_within: std::time::Duration,
) -> Result<bool, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT EXISTS (
               SELECT 1
               FROM all_servable_videos_on_yral v
               LEFT JOIN servable_video_hashes h
                 ON h.video_id = v.video_id
                AND h.hash_kind = $1 AND h.hash_version = $2 AND h.input_media_version = $3
               WHERE h.video_id IS NULL
                 AND NOT EXISTS (
                   SELECT 1 FROM media_job_failures f
                   WHERE f.video_id = v.video_id
                     AND f.created_at > now() - ($4::double precision * interval '1 second')
                 )
               LIMIT 1
             ) AS eligible",
            &[
                &hash_kind,
                &hash_version,
                &input_media_version,
                &(failed_within.as_secs_f64()),
            ],
        )
        .await?;
    Ok(row.get("eligible"))
}

/// A single row from `media_job_runs`, returned by [`recent_job_runs`].
#[derive(Debug, Clone)]
pub struct JobRunRow {
    pub job_kind: String,
    pub status: String,
    pub requested_by: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub totals: Option<serde_json::Value>,
    pub error_message: Option<String>,
}

/// Return the most-recent job runs, newest first.
///
/// `job_kind` — when `Some`, filters to that job kind only.
/// `limit` — maximum number of rows to return (passed as `LIMIT`).
pub async fn recent_job_runs(
    client: &Client,
    job_kind: Option<&str>,
    limit: i64,
) -> Result<Vec<JobRunRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT job_kind, status, requested_by, started_at, finished_at, totals, error_message
             FROM media_job_runs
             WHERE ($1::TEXT IS NULL OR job_kind = $1)
             ORDER BY started_at DESC
             LIMIT $2",
            &[&job_kind, &limit],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| JobRunRow {
            job_kind: r.get(0),
            status: r.get(1),
            requested_by: r.get(2),
            started_at: r.get(3),
            finished_at: r.get(4),
            totals: r.get(5),
            error_message: r.get(6),
        })
        .collect())
}

/// A group of failures sharing the same `phase`, returned by [`failure_summary`].
#[derive(Debug, Clone)]
pub struct FailureGroup {
    pub phase: String,
    pub count: i64,
    /// Up to 5 distinct `last_error` snippets (≤200 chars each), most-recent first.
    pub samples: Vec<String>,
}

/// Summarise `media_job_failures` by phase.
///
/// Uses two queries per phase to avoid the invalid
/// `SELECT DISTINCT … ORDER BY created_at` form in Postgres:
/// first a `GROUP BY` for counts, then a `LIMIT 50` fetch whose results are
/// deduped to at most 5 distinct error strings in Rust.
pub async fn failure_summary(
    client: &Client,
    job_kind: Option<&str>,
    limit: i64,
) -> Result<Vec<FailureGroup>, tokio_postgres::Error> {
    // Query 1: counts per phase.
    let count_rows = client
        .query(
            "SELECT phase, COUNT(*) FROM media_job_failures
             WHERE ($1::TEXT IS NULL OR job_kind = $1)
             GROUP BY phase ORDER BY COUNT(*) DESC LIMIT $2",
            &[&job_kind, &limit],
        )
        .await?;

    let mut out = Vec::with_capacity(count_rows.len());
    for cr in count_rows {
        let phase: String = cr.get(0);
        let count: i64 = cr.get(1);
        // Query 2: recent rows for this phase; dedup to <=5 distinct in Rust
        // (avoids the invalid `SELECT DISTINCT ... ORDER BY created_at` form).
        let sample_rows = client
            .query(
                "SELECT left(last_error, 200) FROM media_job_failures
                 WHERE phase = $1 AND ($2::TEXT IS NULL OR job_kind = $2)
                 ORDER BY created_at DESC LIMIT 50",
                &[&phase, &job_kind],
            )
            .await?;
        let mut samples: Vec<String> = Vec::with_capacity(5);
        for sr in sample_rows {
            let s: String = sr.get(0);
            if !samples.contains(&s) {
                samples.push(s);
            }
            if samples.len() == 5 {
                break;
            }
        }
        out.push(FailureGroup {
            phase,
            count,
            samples,
        });
    }
    Ok(out)
}

/// Flush live progress totals into an in-progress `media_job_runs` row.
///
/// Called best-effort after each page (never aborts the job on failure).
/// The final accurate totals are written by `complete_job_run`; this helper
/// makes the row readable while the job is still running.
pub async fn update_job_run_totals(
    client: &Client,
    job_run_id: uuid::Uuid,
    totals: &serde_json::Value,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE media_job_runs SET totals = $2 WHERE id = $1::TEXT::UUID",
            &[&job_run_id.to_string(), totals],
        )
        .await?;
    Ok(())
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

/// A snapshot of the single-row `sweep_lease` election lease (`id = 1`),
/// returned by [`read_lease`].
#[derive(Debug, Clone)]
pub struct LeaseRow {
    pub owner: String,
    pub heartbeat: chrono::DateTime<chrono::Utc>,
    pub last_discovery_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Atomically acquire the `sweep_lease` for `owner`, or renew it if `owner`
/// already holds it, or steal it if the current holder's heartbeat is older
/// than `ttl`.
///
/// Implemented as a single `INSERT ... ON CONFLICT DO UPDATE ... RETURNING`
/// statement (one txn) so it is safe under pgbouncer transaction pooling — no
/// multi-statement session state is required.
///
/// Returns `Ok(true)` iff `owner` now holds the lease. When the row exists with
/// a *different, fresh* owner, the conditional `UPDATE` is skipped, no row is
/// returned, and this returns `Ok(false)` ("another box holds it, skip").
pub async fn acquire_or_renew_lease(
    client: &Client,
    owner: &str,
    ttl: std::time::Duration,
) -> Result<bool, tokio_postgres::Error> {
    let row = client
        .query_opt(
            "INSERT INTO sweep_lease (id, owner, heartbeat) VALUES (1, $1, now())
             ON CONFLICT (id) DO UPDATE
                SET owner = $1, heartbeat = now()
                WHERE sweep_lease.owner = $1
                   OR sweep_lease.heartbeat < now() - ($2::double precision * interval '1 second')
             RETURNING owner",
            &[&owner, &(ttl.as_secs_f64())],
        )
        .await?;
    Ok(row.is_some())
}

/// Release the `sweep_lease` iff it is currently held by `owner`.
///
/// Deletes the `id = 1` row only when `owner` matches, so a box can never
/// release a lease that another box has since stolen.
pub async fn release_lease(client: &Client, owner: &str) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "DELETE FROM sweep_lease WHERE id = 1 AND owner = $1",
            &[&owner],
        )
        .await?;
    Ok(())
}

/// Read the current `sweep_lease` row (`id = 1`), or `None` if unheld.
pub async fn read_lease(client: &Client) -> Result<Option<LeaseRow>, tokio_postgres::Error> {
    let row = client
        .query_opt(
            "SELECT owner, heartbeat, last_discovery_at FROM sweep_lease WHERE id = 1",
            &[],
        )
        .await?;
    Ok(row.map(|r| LeaseRow {
        owner: r.get(0),
        heartbeat: r.get(1),
        last_discovery_at: r.get(2),
    }))
}

/// Read `last_discovery_at` from the `sweep_lease` row (`id = 1`).
///
/// Returns `None` when there is no lease row OR the column is `NULL`.
pub async fn get_last_discovery_at(
    client: &Client,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, tokio_postgres::Error> {
    let row = client
        .query_opt(
            "SELECT last_discovery_at FROM sweep_lease WHERE id = 1",
            &[],
        )
        .await?;
    Ok(row.and_then(|r| r.get(0)))
}

/// Record the timestamp of the most recent discovery sweep on the
/// `sweep_lease` row (`id = 1`).
pub async fn set_last_discovery_at(
    client: &Client,
    ts: chrono::DateTime<chrono::Utc>,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE sweep_lease SET last_discovery_at = $1 WHERE id = 1",
            &[&ts],
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{acquire_or_renew_lease, get_last_discovery_at, read_lease, set_last_discovery_at};
    use crate::media_index::test_support::test_client;

    #[tokio::test]
    async fn lease_single_owner_and_steal_after_ttl() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        let ttl = std::time::Duration::from_secs(60);
        assert!(
            acquire_or_renew_lease(&client, "box-a", ttl).await.unwrap(),
            "first acquire"
        );
        assert!(
            !acquire_or_renew_lease(&client, "box-b", ttl).await.unwrap(),
            "fresh foreign lease -> skip"
        );
        assert!(
            acquire_or_renew_lease(&client, "box-a", ttl).await.unwrap(),
            "owner renews own"
        );

        // Simulate staleness: backdate heartbeat beyond ttl.
        client
            .execute(
                "UPDATE sweep_lease SET heartbeat = now() - interval '120 seconds' WHERE id = 1",
                &[],
            )
            .await
            .unwrap();
        assert!(
            acquire_or_renew_lease(&client, "box-b", ttl).await.unwrap(),
            "stale lease stolen"
        );
        let lease = read_lease(&client).await.unwrap().unwrap();
        assert_eq!(lease.owner, "box-b");
    }

    #[tokio::test]
    async fn last_discovery_at_roundtrip() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        acquire_or_renew_lease(&client, "box-a", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert!(get_last_discovery_at(&client).await.unwrap().is_none());
        let now = chrono::Utc::now();
        set_last_discovery_at(&client, now).await.unwrap();
        let got = get_last_discovery_at(&client).await.unwrap().unwrap();
        assert!((got - now).num_seconds().abs() < 2);
    }

    #[tokio::test]
    async fn any_eligible_for_hash_quarantines_recent_failures() {
        use phash::{HASH_KIND, HASH_VERSION};
        let imv = crate::jobs::media_phash::INPUT_MEDIA_VERSION;
        let within = std::time::Duration::from_secs(86400);
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        // Empty set: nothing eligible.
        assert!(
            !super::any_eligible_for_hash(&client, HASH_KIND, HASH_VERSION, imv, within)
                .await
                .unwrap(),
            "no servable rows -> not eligible"
        );

        // One missing (unhashed) servable row: eligible.
        let tx = client.transaction().await.unwrap();
        crate::media_index::upsert_servable_video_txn(&tx, servable_input("vid-elig"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(
            super::any_eligible_for_hash(&client, HASH_KIND, HASH_VERSION, imv, within)
                .await
                .unwrap(),
            "missing + no failure -> eligible"
        );

        // Recent failure: quarantined (not eligible).
        client
            .execute(
                "INSERT INTO media_job_failures (job_kind, item_key, video_id, phase, last_error)
                 VALUES ('media_phash','vid-elig','vid-elig','phash_download','x')",
                &[],
            )
            .await
            .unwrap();
        assert!(
            !super::any_eligible_for_hash(&client, HASH_KIND, HASH_VERSION, imv, within)
                .await
                .unwrap(),
            "recently-failed -> not eligible"
        );

        // Failure older than the window: eligible again.
        client
            .execute(
                "UPDATE media_job_failures SET created_at = now() - interval '48 hours' WHERE video_id = 'vid-elig'",
                &[],
            )
            .await
            .unwrap();
        assert!(
            super::any_eligible_for_hash(&client, HASH_KIND, HASH_VERSION, imv, within)
                .await
                .unwrap(),
            "stale failure -> eligible again"
        );
    }

    #[tokio::test]
    async fn update_job_run_totals_reflects_progress_before_completion() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        let id = uuid::Uuid::new_v4();
        client
            .execute(
                "INSERT INTO media_job_runs (id, job_kind, status, requested_by) VALUES ($1::TEXT::UUID,'media_phash','running','t')",
                &[&id.to_string()],
            )
            .await
            .unwrap();
        super::update_job_run_totals(&client, id, &serde_json::json!({"scanned_rows": 42}))
            .await
            .unwrap();
        let got: serde_json::Value = client
            .query_one(
                "SELECT totals FROM media_job_runs WHERE id=$1::TEXT::UUID",
                &[&id.to_string()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(got["scanned_rows"], 42);
        // still running (not completed)
        let status: String = client
            .query_one(
                "SELECT status FROM media_job_runs WHERE id=$1::TEXT::UUID",
                &[&id.to_string()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(status, "running");
    }

    #[tokio::test]
    async fn recent_job_runs_returns_newest_first_with_totals() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        for (i, kind) in ["legacy_video_index_import", "media_phash", "media_phash"]
            .iter()
            .enumerate()
        {
            client
                .execute(
                    "INSERT INTO media_job_runs (id, job_kind, status, requested_by, started_at, totals)
                     VALUES (gen_random_uuid(), $1, 'succeeded', 't', now() + ($2 || ' seconds')::interval, '{\"scanned_rows\":7}'::jsonb)",
                    &[kind, &i.to_string()],
                )
                .await
                .unwrap();
        }
        let all = super::recent_job_runs(&client, None, 10).await.unwrap();
        assert_eq!(all.len(), 3);
        assert!(all[0].started_at >= all[1].started_at, "newest first");
        assert_eq!(all[0].totals.as_ref().unwrap()["scanned_rows"], 7);
        let phash = super::recent_job_runs(&client, Some("media_phash"), 10)
            .await
            .unwrap();
        assert_eq!(phash.len(), 2);
    }

    #[tokio::test]
    async fn failure_summary_groups_by_phase_with_distinct_samples() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        // 3 phash_download failures (2 distinct errors), 1 phash_decode
        let rows = [
            ("v1", "phash_download", "storj download a.mp4: 404"),
            ("v2", "phash_download", "storj download b.mp4: 404"),
            ("v3", "phash_download", "storj download a.mp4: 404"),
            ("v4", "phash_decode", "phash: no video stream"),
        ];
        for (vid, phase, err) in rows {
            client
                .execute(
                    "INSERT INTO media_job_failures (job_kind, item_key, video_id, phase, last_error)
                     VALUES ('media_phash', $1, $1, $2, $3)",
                    &[&vid, &phase, &err],
                )
                .await
                .unwrap();
        }
        let groups = super::failure_summary(&client, Some("media_phash"), 10)
            .await
            .unwrap();
        let dl = groups.iter().find(|g| g.phase == "phash_download").unwrap();
        assert_eq!(dl.count, 3);
        // exactly 2 distinct error strings were inserted for phash_download
        assert_eq!(dl.samples.len(), 2);
        // samples are distinct
        let mut s = dl.samples.clone();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), dl.samples.len(), "samples must be distinct");
        assert!(groups
            .iter()
            .any(|g| g.phase == "phash_decode" && g.count == 1));
    }

    fn servable_input(video_id: &str) -> crate::media_index::ServableVideoInput<'_> {
        crate::media_index::ServableVideoInput {
            video_id,
            publisher_user_id: None,
            post_id: None,
            source_kind: "legacy_video_index",
            source_ref: Some(video_id),
            servable_status: "servable",
            nsfw_state: None,
            storage_provider: Some("storj"),
            bucket: None,
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

    #[tokio::test]
    async fn missing_phash_scan_shards_are_disjoint_and_total() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        // Seed 30 servable rows, none hashed. video_ids chosen to spread across buckets;
        // include ones whose hashtext is negative (plain ints stringified is fine — hashtext varies in sign).
        for i in 0..30u32 {
            let vid = format!("shard-vid-{i:04}");
            let tx = client.transaction().await.unwrap();
            crate::media_index::upsert_servable_video_txn(&tx, servable_input(&vid))
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        let of = 3i64;
        let mut all = std::collections::BTreeSet::new();
        let mut total = 0;
        for idx in 0..of {
            let rows = super::videos_missing_canonical_phash(
                &client,
                "phash",
                "offchain_binary_10x8_v1",
                "current_stored_object_v1",
                None,
                None,
                Some((of, idx)),
            )
            .await
            .unwrap();
            for r in &rows {
                assert!(
                    all.insert(r.video_id.clone()),
                    "video {} appeared in two shards",
                    r.video_id
                );
            }
            total += rows.len();
        }
        assert_eq!(
            total, 30,
            "every row lands in exactly one shard (disjoint + total)"
        );
    }

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

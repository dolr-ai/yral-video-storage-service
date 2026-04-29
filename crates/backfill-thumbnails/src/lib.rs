use chrono::{DateTime, NaiveDate, Utc};
use clap::{error::ErrorKind, Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendKind {
    Storj,
    Hetzner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Scope {
    #[value(name = "storj")]
    Storj,
    #[value(name = "hetzner")]
    Hetzner,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Storj => "storj",
            Self::Hetzner => "hetzner",
        }
    }

    pub fn backend(self) -> BackendKind {
        match self {
            Self::Storj => BackendKind::Storj,
            Self::Hetzner => BackendKind::Hetzner,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandKind {
    SeedTestData,
    Run,
    Verify,
    Audit,
}

impl CommandKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SeedTestData => "seed-test-data",
            Self::Run => "run",
            Self::Verify => "verify",
            Self::Audit => "audit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    pub command: CommandKind,
    pub scope: Scope,
    pub manifest_dir: String,
    pub prefix: Option<String>,
    pub bucket_override: Option<String>,
    pub cutoff_before: Option<DateTime<Utc>>,
    pub execute: bool,
    pub download_concurrency: usize,
    pub ffmpeg_concurrency: usize,
    pub upload_concurrency: usize,
    pub verify_sample: Option<usize>,
    pub thumbnail_seek: Option<String>,
    pub seed_count: usize,
    pub shard_index: usize,
    pub shard_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub key: String,
    pub last_modified: DateTime<Utc>,
}

#[derive(Debug, Parser)]
#[command(
    name = "backfill-thumbnails",
    about = "Generate staged first-frame thumbnails for historical videos",
    disable_help_subcommand = true
)]
struct RawCli {
    #[command(subcommand)]
    command: RawCommand,
}

#[derive(Debug, Subcommand)]
enum RawCommand {
    SeedTestData(RawSeedTestDataArgs),
    Run(RawRunArgs),
    Verify(RawVerifyArgs),
    Audit(RawAuditArgs),
}

#[derive(Debug, Args, Clone)]
struct RawCommonArgs {
    #[arg(long, value_enum)]
    scope: Scope,
    #[arg(long, default_value = "artifacts/backfill")]
    manifest_dir: String,
    #[arg(long)]
    prefix: Option<String>,
    #[arg(long = "bucket")]
    bucket_override: Option<String>,
}

#[derive(Debug, Args, Clone)]
struct RawConcurrencyArgs {
    #[arg(long, default_value_t = 8)]
    download_concurrency: usize,
    #[arg(long, default_value_t = 1)]
    ffmpeg_concurrency: usize,
    #[arg(long, default_value_t = 8)]
    upload_concurrency: usize,
}

#[derive(Debug, Args, Clone)]
struct RawShardArgs {
    /// Which slice of videos this job processes (0-based).
    #[arg(long, default_value_t = 0)]
    shard_index: usize,
    /// Total number of parallel shards. 1 means process everything.
    #[arg(long, default_value_t = 1)]
    shard_count: usize,
}

#[derive(Debug, Args)]
struct RawSeedTestDataArgs {
    #[command(flatten)]
    common: RawCommonArgs,
    #[arg(long, default_value_t = 3)]
    seed_count: usize,
}

#[derive(Debug, Args)]
struct RawRunArgs {
    #[command(flatten)]
    common: RawCommonArgs,
    #[arg(long)]
    cutoff_before: String,
    #[arg(long)]
    execute: bool,
    #[command(flatten)]
    concurrency: RawConcurrencyArgs,
    #[command(flatten)]
    shard: RawShardArgs,
    #[arg(long)]
    thumbnail_seek: Option<String>,
}

#[derive(Debug, Args)]
struct RawVerifyArgs {
    #[command(flatten)]
    common: RawCommonArgs,
    #[arg(long)]
    cutoff_before: String,
    #[command(flatten)]
    concurrency: RawConcurrencyArgs,
    #[arg(long)]
    verify_sample: Option<usize>,
}

#[derive(Debug, Args)]
struct RawAuditArgs {
    #[command(flatten)]
    common: RawCommonArgs,
    #[arg(long)]
    cutoff_before: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedAction {
    Process {
        video_key: String,
        staged_thumbnail_key: String,
    },
    SkipRemoteExists {
        video_key: String,
        staged_thumbnail_key: String,
    },
    SkipManifestDone {
        video_key: String,
        staged_thumbnail_key: String,
    },
}

impl FromStr for Scope {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "storj" => Ok(Self::Storj),
            "hetzner" => Ok(Self::Hetzner),
            other => Err(format!("unsupported scope: {other}")),
        }
    }
}

pub fn parse_cli_args(args: &[String]) -> Result<CliOptions, clap::Error> {
    RawCli::try_parse_from(args.iter().cloned())?.into_cli_options()
}

impl RawCli {
    fn into_cli_options(self) -> Result<CliOptions, clap::Error> {
        self.command.into_cli_options()
    }
}

impl RawCommand {
    fn into_cli_options(self) -> Result<CliOptions, clap::Error> {
        match self {
            Self::SeedTestData(args) => {
                validate_scope_bucket(args.common.scope, args.common.bucket_override.as_deref())?;
                Ok(CliOptions {
                    command: CommandKind::SeedTestData,
                    scope: args.common.scope,
                    manifest_dir: args.common.manifest_dir,
                    prefix: args.common.prefix,
                    bucket_override: args.common.bucket_override,
                    cutoff_before: None,
                    execute: false,
                    download_concurrency: 8,
                    ffmpeg_concurrency: 1,
                    upload_concurrency: 8,
                    verify_sample: None,
                    thumbnail_seek: None,
                    seed_count: args.seed_count,
                    shard_index: 0,
                    shard_count: 1,
                })
            }
            Self::Run(args) => {
                validate_scope_bucket(args.common.scope, args.common.bucket_override.as_deref())?;
                validate_shard(args.shard.shard_index, args.shard.shard_count)?;
                Ok(CliOptions {
                    command: CommandKind::Run,
                    scope: args.common.scope,
                    manifest_dir: args.common.manifest_dir,
                    prefix: args.common.prefix,
                    bucket_override: args.common.bucket_override,
                    cutoff_before: Some(parse_cutoff_arg(&args.cutoff_before)?),
                    execute: args.execute,
                    download_concurrency: args.concurrency.download_concurrency,
                    ffmpeg_concurrency: args.concurrency.ffmpeg_concurrency,
                    upload_concurrency: args.concurrency.upload_concurrency,
                    verify_sample: None,
                    thumbnail_seek: args.thumbnail_seek,
                    seed_count: 3,
                    shard_index: args.shard.shard_index,
                    shard_count: args.shard.shard_count,
                })
            }
            Self::Verify(args) => {
                validate_scope_bucket(args.common.scope, args.common.bucket_override.as_deref())?;
                Ok(CliOptions {
                    command: CommandKind::Verify,
                    scope: args.common.scope,
                    manifest_dir: args.common.manifest_dir,
                    prefix: args.common.prefix,
                    bucket_override: args.common.bucket_override,
                    cutoff_before: Some(parse_cutoff_arg(&args.cutoff_before)?),
                    execute: false,
                    download_concurrency: args.concurrency.download_concurrency,
                    ffmpeg_concurrency: args.concurrency.ffmpeg_concurrency,
                    upload_concurrency: args.concurrency.upload_concurrency,
                    verify_sample: args.verify_sample,
                    thumbnail_seek: None,
                    seed_count: 3,
                    shard_index: 0,
                    shard_count: 1,
                })
            }
            Self::Audit(args) => {
                validate_scope_bucket(args.common.scope, args.common.bucket_override.as_deref())?;
                Ok(CliOptions {
                    command: CommandKind::Audit,
                    scope: args.common.scope,
                    manifest_dir: args.common.manifest_dir,
                    prefix: args.common.prefix,
                    bucket_override: args.common.bucket_override,
                    cutoff_before: Some(parse_cutoff_arg(&args.cutoff_before)?),
                    execute: false,
                    download_concurrency: 8,
                    ffmpeg_concurrency: 1,
                    upload_concurrency: 8,
                    verify_sample: None,
                    thumbnail_seek: None,
                    seed_count: 3,
                    shard_index: 0,
                    shard_count: 1,
                })
            }
        }
    }
}

/// FNV-1a 64-bit hash — stable, dependency-free, good distribution for key strings.
pub fn shard_for_key(key: &str, shard_count: usize) -> usize {
    if shard_count <= 1 {
        return 0;
    }
    let mut hash: u64 = 14695981039346656037;
    for byte in key.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    (hash % shard_count as u64) as usize
}

fn validate_shard(shard_index: usize, shard_count: usize) -> Result<(), clap::Error> {
    if shard_count == 0 {
        return Err(value_validation_error("--shard-count must be at least 1"));
    }
    if shard_index >= shard_count {
        return Err(value_validation_error(&format!(
            "--shard-index {shard_index} must be less than --shard-count {shard_count}"
        )));
    }
    Ok(())
}

fn validate_scope_bucket(scope: Scope, bucket_override: Option<&str>) -> Result<(), clap::Error> {
    if scope == Scope::Storj && bucket_override.is_none() {
        return Err(missing_required_argument_error(
            "--bucket is required for storj scope".to_string(),
        ));
    }

    Ok(())
}

fn missing_required_argument_error(message: String) -> clap::Error {
    clap::Error::raw(ErrorKind::MissingRequiredArgument, message)
}

fn value_validation_error(message: &str) -> clap::Error {
    clap::Error::raw(ErrorKind::ValueValidation, message)
}

fn parse_cutoff_arg(value: &str) -> Result<DateTime<Utc>, clap::Error> {
    parse_cutoff(value).map_err(|message| value_validation_error(&message))
}

fn parse_cutoff(value: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(date_time) = DateTime::parse_from_rfc3339(value) {
        return Ok(date_time.with_timezone(&Utc));
    }

    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|err| format!("invalid cutoff value: {err}"))?;
    let date_time = date
        .and_hms_opt(23, 59, 59)
        .ok_or_else(|| "invalid cutoff time".to_string())?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(date_time, Utc))
}

pub fn build_run_plan(
    backend: BackendKind,
    videos: &[ObjectInfo],
    remote_staged_keys: &HashSet<String>,
    manifest: &ManifestIndex,
    bucket: &str,
    cutoff_before: DateTime<Utc>,
    prefix: Option<&str>,
) -> Vec<PlannedAction> {
    let mut actions = Vec::new();

    for video in videos {
        if !video.key.ends_with(".mp4") {
            continue;
        }
        if !is_before_cutoff(video.last_modified, cutoff_before) {
            continue;
        }
        if let Some(prefix) = prefix {
            if !video.key.starts_with(prefix) {
                continue;
            }
        }

        let Some(staged_thumbnail_key) = staged_thumbnail_key(&video.key) else {
            continue;
        };

        if remote_staged_keys.contains(&staged_thumbnail_key) {
            actions.push(PlannedAction::SkipRemoteExists {
                video_key: video.key.clone(),
                staged_thumbnail_key,
            });
            continue;
        }

        if manifest.should_skip_completed(backend, bucket, &staged_thumbnail_key) {
            actions.push(PlannedAction::SkipManifestDone {
                video_key: video.key.clone(),
                staged_thumbnail_key,
            });
            continue;
        }

        actions.push(PlannedAction::Process {
            video_key: video.key.clone(),
            staged_thumbnail_key,
        });
    }

    actions
}

pub fn staged_thumbnail_key(video_key: &str) -> Option<String> {
    let video_id = video_key.strip_suffix(".mp4")?;
    Some(format!("{video_id}-thumbnail.png"))
}

pub fn is_before_cutoff(last_modified: DateTime<Utc>, cutoff: DateTime<Utc>) -> bool {
    last_modified <= cutoff
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManifestStatus {
    Completed,
    Failed,
    SkipRemoteExists,
    SkipManifestDone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub backend: BackendKind,
    pub bucket: String,
    pub source_video_key: String,
    pub staged_thumbnail_key: String,
    pub status: ManifestStatus,
}

#[derive(Debug, Default)]
pub struct ManifestIndex {
    completed: HashSet<(BackendKind, String, String)>,
}

impl ManifestIndex {
    pub fn from_entries(entries: &[ManifestEntry]) -> Self {
        let completed = entries
            .iter()
            .filter(|entry| entry.status == ManifestStatus::Completed)
            .map(|entry| {
                (
                    entry.backend,
                    entry.bucket.clone(),
                    entry.staged_thumbnail_key.clone(),
                )
            })
            .collect();

        Self { completed }
    }

    pub fn should_skip_completed(
        &self,
        backend: BackendKind,
        bucket: &str,
        staged_thumbnail_key: &str,
    ) -> bool {
        self.completed.contains(&(
            backend,
            bucket.to_string(),
            staged_thumbnail_key.to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn shard_for_key_with_count_one_always_returns_zero() {
        assert_eq!(shard_for_key("any/key.mp4", 1), 0);
        assert_eq!(shard_for_key("another/key.mp4", 0), 0);
    }

    #[test]
    fn shard_for_key_distributes_evenly_across_three_shards() {
        let keys: Vec<String> = (0..300)
            .map(|i| format!("publisher-{i:04x}/video-{i:032x}.mp4"))
            .collect();
        let mut counts = [0usize; 3];
        for key in &keys {
            counts[shard_for_key(key, 3)] += 1;
        }
        // Each shard should get roughly 100 ± 30 (within 30% of ideal)
        for count in counts {
            assert!(count > 70 && count < 130, "uneven shard: {count}");
        }
    }

    #[test]
    fn validate_shard_rejects_index_ge_count() {
        assert!(validate_shard(3, 3).is_err());
        assert!(validate_shard(0, 0).is_err());
        assert!(validate_shard(0, 1).is_ok());
        assert!(validate_shard(2, 3).is_ok());
    }

    #[test]
    fn parses_scope_and_backend_shape() {
        assert_eq!(
            <Scope as FromStr>::from_str("storj").expect("scope parse"),
            Scope::Storj
        );
        assert_eq!(Scope::Storj.backend(), BackendKind::Storj);
        assert_eq!(Scope::Hetzner.backend(), BackendKind::Hetzner);
    }

    #[test]
    fn derives_staged_thumbnail_key_in_same_directory() {
        assert_eq!(
            staged_thumbnail_key("publisher/video-123.mp4").as_deref(),
            Some("publisher/video-123-thumbnail.png")
        );
        assert_eq!(
            staged_thumbnail_key("video-123.mp4").as_deref(),
            Some("video-123-thumbnail.png")
        );
        assert_eq!(staged_thumbnail_key("video-123.mov"), None);
    }

    #[test]
    fn cutoff_accepts_older_objects_and_rejects_newer_ones() {
        let cutoff = DateTime::parse_from_rfc3339("2026-04-22T00:00:00Z")
            .expect("cutoff parse")
            .with_timezone(&Utc);
        let old = DateTime::parse_from_rfc3339("2026-04-21T23:59:59Z")
            .expect("old parse")
            .with_timezone(&Utc);
        let newer = DateTime::parse_from_rfc3339("2026-04-22T00:00:01Z")
            .expect("newer parse")
            .with_timezone(&Utc);

        assert!(is_before_cutoff(old, cutoff));
        assert!(!is_before_cutoff(newer, cutoff));
    }

    #[test]
    fn manifest_index_only_skips_completed_entries() {
        let index = ManifestIndex::from_entries(&[
            ManifestEntry {
                backend: BackendKind::Storj,
                bucket: "bucket-a".to_string(),
                source_video_key: "publisher/video-123.mp4".to_string(),
                staged_thumbnail_key: "publisher/video-123-thumbnail.png".to_string(),
                status: ManifestStatus::Completed,
            },
            ManifestEntry {
                backend: BackendKind::Storj,
                bucket: "bucket-a".to_string(),
                source_video_key: "publisher/video-456.mp4".to_string(),
                staged_thumbnail_key: "publisher/video-456-thumbnail.png".to_string(),
                status: ManifestStatus::Failed,
            },
        ]);

        assert!(index.should_skip_completed(
            BackendKind::Storj,
            "bucket-a",
            "publisher/video-123-thumbnail.png"
        ));
        assert!(!index.should_skip_completed(
            BackendKind::Storj,
            "bucket-a",
            "publisher/video-456-thumbnail.png"
        ));
        assert!(!index.should_skip_completed(
            BackendKind::Hetzner,
            "bucket-a",
            "publisher/video-123-thumbnail.png"
        ));
        assert!(!index.should_skip_completed(
            BackendKind::Storj,
            "bucket-b",
            "publisher/video-123-thumbnail.png"
        ));
    }

    #[test]
    fn parses_run_command_with_defaults() {
        let cli = parse_cli_args(&[
            "backfill-thumbnails".to_string(),
            "run".to_string(),
            "--scope".to_string(),
            "storj".to_string(),
            "--bucket".to_string(),
            "yral-videos".to_string(),
            "--cutoff-before".to_string(),
            "2026-04-22T00:00:00Z".to_string(),
        ])
        .expect("cli parse");

        assert_eq!(cli.command, CommandKind::Run);
        assert_eq!(cli.scope, Scope::Storj);
        assert_eq!(cli.manifest_dir, "artifacts/backfill");
        assert!(!cli.execute);
        assert_eq!(cli.download_concurrency, 8);
        assert_eq!(cli.ffmpeg_concurrency, 1);
        assert_eq!(cli.upload_concurrency, 8);
        assert_eq!(
            cli.cutoff_before,
            Some(
                DateTime::parse_from_rfc3339("2026-04-22T00:00:00Z")
                    .expect("cutoff parse")
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn parses_seed_test_data_command_with_count_override() {
        let cli = parse_cli_args(&[
            "backfill-thumbnails".to_string(),
            "seed-test-data".to_string(),
            "--scope".to_string(),
            "hetzner".to_string(),
            "--seed-count".to_string(),
            "5".to_string(),
            "--bucket".to_string(),
            "test-bucket".to_string(),
        ])
        .expect("cli parse");

        assert_eq!(cli.command, CommandKind::SeedTestData);
        assert_eq!(cli.scope, Scope::Hetzner);
        assert_eq!(cli.seed_count, 5);
        assert_eq!(cli.bucket_override.as_deref(), Some("test-bucket"));
        assert_eq!(cli.thumbnail_seek, None);
    }

    #[test]
    fn requires_explicit_bucket_for_storj_scope() {
        let error = parse_cli_args(&[
            "backfill-thumbnails".to_string(),
            "run".to_string(),
            "--scope".to_string(),
            "storj".to_string(),
            "--cutoff-before".to_string(),
            "2026-04-22T00:00:00Z".to_string(),
        ])
        .expect_err("expected parse rejection");

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        assert!(error.to_string().contains("--bucket"));
    }

    #[test]
    fn top_level_help_returns_usage_text() {
        let error = parse_cli_args(&["backfill-thumbnails".to_string(), "--help".to_string()])
            .expect_err("expected help output");

        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        let rendered = error.to_string();
        assert!(rendered.contains("Usage:"));
        assert!(rendered.contains("seed-test-data"));
        assert!(rendered.contains("verify"));
    }

    #[test]
    fn run_subcommand_help_returns_usage_text() {
        let error = parse_cli_args(&[
            "backfill-thumbnails".to_string(),
            "run".to_string(),
            "--help".to_string(),
        ])
        .expect_err("expected subcommand help output");

        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        let rendered = error.to_string();
        assert!(rendered.contains("Usage:"));
        assert!(rendered.contains("--cutoff-before"));
        assert!(rendered.contains("--download-concurrency"));
    }

    #[test]
    fn build_run_plan_prefers_remote_skip_then_manifest_skip_then_process() {
        let cutoff = DateTime::parse_from_rfc3339("2026-04-22T00:00:00Z")
            .expect("cutoff parse")
            .with_timezone(&Utc);
        let videos = vec![
            ObjectInfo {
                key: "publisher/already-remote.mp4".to_string(),
                last_modified: cutoff,
            },
            ObjectInfo {
                key: "publisher/already-manifest.mp4".to_string(),
                last_modified: cutoff,
            },
            ObjectInfo {
                key: "publisher/to-process.mp4".to_string(),
                last_modified: cutoff,
            },
            ObjectInfo {
                key: "publisher/too-new.mp4".to_string(),
                last_modified: DateTime::parse_from_rfc3339("2026-04-22T00:00:01Z")
                    .expect("newer parse")
                    .with_timezone(&Utc),
            },
            ObjectInfo {
                key: "other/prefix-miss.mp4".to_string(),
                last_modified: cutoff,
            },
        ];
        let remote = HashSet::from([String::from("publisher/already-remote-thumbnail.png")]);
        let manifest = ManifestIndex::from_entries(&[ManifestEntry {
            backend: BackendKind::Storj,
            bucket: "test-bucket-a".to_string(),
            source_video_key: "publisher/already-manifest.mp4".to_string(),
            staged_thumbnail_key: "publisher/already-manifest-thumbnail.png".to_string(),
            status: ManifestStatus::Completed,
        }]);

        let plan = build_run_plan(
            BackendKind::Storj,
            &videos,
            &remote,
            &manifest,
            "test-bucket-a",
            cutoff,
            Some("publisher/"),
        );

        assert_eq!(
            plan,
            vec![
                PlannedAction::SkipRemoteExists {
                    video_key: "publisher/already-remote.mp4".to_string(),
                    staged_thumbnail_key: "publisher/already-remote-thumbnail.png".to_string(),
                },
                PlannedAction::SkipManifestDone {
                    video_key: "publisher/already-manifest.mp4".to_string(),
                    staged_thumbnail_key: "publisher/already-manifest-thumbnail.png".to_string(),
                },
                PlannedAction::Process {
                    video_key: "publisher/to-process.mp4".to_string(),
                    staged_thumbnail_key: "publisher/to-process-thumbnail.png".to_string(),
                },
            ]
        );
    }

    #[test]
    fn manifest_index_distinguishes_buckets_for_same_backend() {
        let index = ManifestIndex::from_entries(&[ManifestEntry {
            backend: BackendKind::Storj,
            bucket: "test-bucket-a".to_string(),
            source_video_key: "publisher/video-123.mp4".to_string(),
            staged_thumbnail_key: "publisher/video-123-thumbnail.png".to_string(),
            status: ManifestStatus::Completed,
        }]);

        assert!(index.should_skip_completed(
            BackendKind::Storj,
            "test-bucket-a",
            "publisher/video-123-thumbnail.png"
        ));
        assert!(!index.should_skip_completed(
            BackendKind::Storj,
            "test-bucket-b",
            "publisher/video-123-thumbnail.png"
        ));
    }
}

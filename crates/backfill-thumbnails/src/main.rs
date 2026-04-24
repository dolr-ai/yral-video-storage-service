mod backend;

use anyhow::{bail, Context, Result};
use backfill_thumbnails::{
    build_run_plan, parse_cli_args, staged_thumbnail_key, CliOptions, CommandKind, ManifestEntry,
    ManifestIndex, ManifestStatus, PlannedAction, Scope,
};
use chrono::Utc;
use image::ImageFormat;
use serde::Serialize;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storj_interface::thumbnail::{extract_thumbnail, extract_thumbnail_with_seek};
use tempfile::tempdir;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

use crate::backend::Backend;

#[derive(Debug, Default, Serialize)]
struct CommandSummary {
    run_id: String,
    command: String,
    scope: String,
    bucket: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    cutoff: String,
    execute: bool,
    total_objects: usize,
    candidate_videos: usize,
    existing_staged_objects: usize,
    planned_process: usize,
    skip_remote_exists: usize,
    skip_manifest_done: usize,
    completed: usize,
    failed: usize,
    dry_run: usize,
    verified_pass: usize,
    verified_fail: usize,
    verified_skip: usize,
    seeded: usize,
}

#[derive(Debug, Serialize)]
struct FailureRecord {
    video_key: String,
    staged_thumbnail_key: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct VerifyRecord {
    video_key: String,
    staged_thumbnail_key: String,
    status: &'static str,
    detail: String,
}

#[derive(Debug, Serialize)]
struct AuditReport {
    scope: String,
    bucket: String,
    total_objects: usize,
    total_candidates_before_cutoff: usize,
    total_existing_staged: usize,
    manifest_completed: usize,
    manifest_failed: usize,
    remaining_work: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args: Vec<String> = env::args().collect();
    let cli = match parse_cli_args(&args) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    let backend = Backend::from_scope(cli.scope, cli.bucket_override.clone()).await?;

    let run_id = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let state_dir =
        state_dir_for_scope_and_bucket(&cli.manifest_dir, cli.scope, backend.bucket_name());
    let run_dir = state_dir
        .join("runs")
        .join(format!("{run_id}-{}", cli.command.as_str()));
    fs::create_dir_all(&run_dir)
        .await
        .with_context(|| format!("create run dir {}", run_dir.display()))?;

    let manifest_path = state_dir.join("manifest.jsonl");
    let summary_path = run_dir.join("summary.json");
    let failures_path = run_dir.join("failures.jsonl");
    let verify_path = run_dir.join("verify.jsonl");
    let audit_path = run_dir.join("audit.json");
    let append_lock = Arc::new(Mutex::new(()));

    let summary = match cli.command {
        CommandKind::SeedTestData => {
            execute_seed_test_data(&cli, &backend, &run_id, &summary_path).await?
        }
        CommandKind::Run => {
            execute_run(
                &cli,
                &backend,
                &run_id,
                &manifest_path,
                &summary_path,
                &failures_path,
                append_lock,
            )
            .await?
        }
        CommandKind::Verify => {
            execute_verify(
                &cli,
                &backend,
                &run_id,
                &summary_path,
                &verify_path,
                append_lock,
            )
            .await?
        }
        CommandKind::Audit => {
            execute_audit(&cli, &backend, &run_id, &summary_path, &audit_path).await?
        }
    };

    println!("{}", render_terminal_summary(&summary));

    if matches!(cli.command, CommandKind::Run) && cli.execute && summary.failed > 0 {
        bail!("{} backfill items failed", summary.failed);
    }
    if matches!(cli.command, CommandKind::Verify) && summary.verified_fail > 0 {
        bail!("{} verification checks failed", summary.verified_fail);
    }

    Ok(())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
}

async fn execute_run(
    cli: &CliOptions,
    backend: &Backend,
    run_id: &str,
    manifest_path: &Path,
    summary_path: &Path,
    failures_path: &Path,
    append_lock: Arc<Mutex<()>>,
) -> Result<CommandSummary> {
    let objects = backend.list_objects(cli.prefix.as_deref()).await?;
    let cutoff = cli
        .cutoff_before
        .context("run command requires cutoff_before")?;
    let remote_staged_keys = objects
        .iter()
        .filter(|object| object.key.ends_with("-thumbnail.png") && !object.key.ends_with("-bak-thumbnail.png"))
        .map(|object| object.key.clone())
        .collect::<std::collections::HashSet<_>>();
    let videos = objects
        .iter()
        .filter(|object| object.key.ends_with(".mp4"))
        .cloned()
        .collect::<Vec<_>>();

    let manifest_entries = load_manifest_entries(manifest_path).await?;
    let manifest = ManifestIndex::from_entries(&manifest_entries);
    let plan = build_run_plan(
        backend.kind(),
        &videos,
        &remote_staged_keys,
        &manifest,
        backend.bucket_name(),
        cutoff,
        cli.prefix.as_deref(),
    );

    let mut summary = CommandSummary {
        run_id: run_id.to_string(),
        command: cli.command.as_str().to_string(),
        scope: cli.scope.as_str().to_string(),
        bucket: backend.bucket_name().to_string(),
        cutoff: cutoff.to_rfc3339(),
        execute: cli.execute,
        total_objects: objects.len(),
        candidate_videos: videos.len(),
        existing_staged_objects: remote_staged_keys.len(),
        ..Default::default()
    };

    for action in &plan {
        match action {
            PlannedAction::Process { .. } => summary.planned_process += 1,
            PlannedAction::SkipRemoteExists { .. } => summary.skip_remote_exists += 1,
            PlannedAction::SkipManifestDone { .. } => summary.skip_manifest_done += 1,
        }
    }

    if !cli.execute {
        summary.dry_run = summary.planned_process;
        write_json(summary_path, &summary).await?;
        return Ok(summary);
    }

    let backend = Arc::new(backend.clone());
    let download_sem = Arc::new(Semaphore::new(cli.download_concurrency.max(1)));
    let ffmpeg_sem = Arc::new(Semaphore::new(cli.ffmpeg_concurrency.max(1)));
    let upload_sem = Arc::new(Semaphore::new(cli.upload_concurrency.max(1)));
    let mut join_set = JoinSet::new();

    for action in plan {
        let PlannedAction::Process {
            video_key,
            staged_thumbnail_key,
        } = action
        else {
            continue;
        };

        let backend = backend.clone();
        let download_sem = download_sem.clone();
        let ffmpeg_sem = ffmpeg_sem.clone();
        let upload_sem = upload_sem.clone();
        let manifest_path = manifest_path.to_path_buf();
        let failures_path = failures_path.to_path_buf();
        let append_lock = append_lock.clone();
        let thumbnail_seek = cli.thumbnail_seek.clone();

        join_set.spawn(async move {
            let result = async {
                let _permit = download_sem.acquire_owned().await?;
                let video_data = backend.download_object(&video_key).await?;
                drop(_permit);

                let _permit = ffmpeg_sem.acquire_owned().await?;
                let thumbnail = extract_thumbnail_with_seek(&video_data, thumbnail_seek.as_deref())
                    .await
                    .context("extract first-frame thumbnail")?;
                drop(_permit);

                let _permit = upload_sem.acquire_owned().await?;
                backend
                    .upload_png(&staged_thumbnail_key, thumbnail)
                    .await
                    .context("upload staged thumbnail")?;
                drop(_permit);

                Ok::<(), anyhow::Error>(())
            }
            .await;

            match result {
                Ok(()) => {
                    let manifest_entry = ManifestEntry {
                        backend: backend.kind(),
                        bucket: backend.bucket_name().to_string(),
                        source_video_key: video_key.clone(),
                        staged_thumbnail_key: staged_thumbnail_key.clone(),
                        status: ManifestStatus::Completed,
                    };
                    append_jsonl(manifest_path.as_path(), &manifest_entry, &append_lock).await?;
                    Ok::<_, anyhow::Error>((true, video_key, staged_thumbnail_key, String::new()))
                }
                Err(err) => {
                    let manifest_entry = ManifestEntry {
                        backend: backend.kind(),
                        bucket: backend.bucket_name().to_string(),
                        source_video_key: video_key.clone(),
                        staged_thumbnail_key: staged_thumbnail_key.clone(),
                        status: ManifestStatus::Failed,
                    };
                    append_jsonl(manifest_path.as_path(), &manifest_entry, &append_lock).await?;
                    let failure = FailureRecord {
                        video_key: video_key.clone(),
                        staged_thumbnail_key: staged_thumbnail_key.clone(),
                        error: format!("{err:#}"),
                    };
                    append_jsonl(failures_path.as_path(), &failure, &append_lock).await?;
                    Ok((false, video_key, staged_thumbnail_key, format!("{err:#}")))
                }
            }
        });
    }

    while let Some(result) = join_set.join_next().await {
        let (success, _, _, _) = result??;
        if success {
            summary.completed += 1;
        } else {
            summary.failed += 1;
        }
    }

    write_json(summary_path, &summary).await?;
    Ok(summary)
}

async fn execute_verify(
    cli: &CliOptions,
    backend: &Backend,
    run_id: &str,
    summary_path: &Path,
    verify_path: &Path,
    append_lock: Arc<Mutex<()>>,
) -> Result<CommandSummary> {
    let objects = backend.list_objects(cli.prefix.as_deref()).await?;
    let cutoff = cli
        .cutoff_before
        .context("verify command requires cutoff_before")?;
    let remote_staged_keys = objects
        .iter()
        .filter(|object| object.key.ends_with("-thumbnail.png") && !object.key.ends_with("-bak-thumbnail.png"))
        .map(|object| object.key.clone())
        .collect::<std::collections::HashSet<_>>();

    let mut candidates = objects
        .iter()
        .filter(|object| object.key.ends_with(".mp4"))
        .filter(|object| object.last_modified <= cutoff)
        .filter(|object| {
            cli.prefix
                .as_deref()
                .map(|prefix| object.key.starts_with(prefix))
                .unwrap_or(true)
        })
        .filter_map(|object| {
            let staged = staged_thumbnail_key(&object.key)?;
            remote_staged_keys
                .contains(&staged)
                .then_some((object.key.clone(), staged))
        })
        .collect::<Vec<_>>();

    if let Some(sample) = cli.verify_sample {
        candidates.truncate(sample);
    }

    let backend = Arc::new(backend.clone());
    let download_sem = Arc::new(Semaphore::new(cli.download_concurrency.max(1)));
    let ffmpeg_sem = Arc::new(Semaphore::new(cli.ffmpeg_concurrency.max(1)));
    let mut join_set = JoinSet::new();
    let mut summary = CommandSummary {
        run_id: run_id.to_string(),
        command: cli.command.as_str().to_string(),
        scope: cli.scope.as_str().to_string(),
        bucket: backend.bucket_name().to_string(),
        cutoff: cutoff.to_rfc3339(),
        execute: false,
        total_objects: objects.len(),
        candidate_videos: candidates.len(),
        existing_staged_objects: remote_staged_keys.len(),
        ..Default::default()
    };

    for (video_key, staged_thumbnail_key) in candidates {
        let backend = backend.clone();
        let download_sem = download_sem.clone();
        let ffmpeg_sem = ffmpeg_sem.clone();
        let verify_path = verify_path.to_path_buf();
        let append_lock = append_lock.clone();

        join_set.spawn(async move {
            let result = async {
                let _permit = download_sem.acquire_owned().await?;
                let video_data = backend.download_object(&video_key).await?;
                let staged_thumbnail = backend.download_object(&staged_thumbnail_key).await?;
                drop(_permit);

                let _permit = ffmpeg_sem.acquire_owned().await?;
                let expected_thumbnail = extract_thumbnail(&video_data)
                    .await
                    .context("extract expected first-frame thumbnail")?;
                drop(_permit);

                compare_png_pixels(&expected_thumbnail, &staged_thumbnail)
            }
            .await;

            let (status, detail) = match result {
                Ok(true) => (
                    "PASS",
                    "staged thumbnail matches decoded first-frame pixels".to_string(),
                ),
                Ok(false) => (
                    "FAIL",
                    "staged thumbnail differs from decoded first-frame pixels".to_string(),
                ),
                Err(err) => ("SKIP", format!("{err:#}")),
            };

            let record = VerifyRecord {
                video_key: video_key.clone(),
                staged_thumbnail_key: staged_thumbnail_key.clone(),
                status,
                detail: detail.clone(),
            };
            append_jsonl(verify_path.as_path(), &record, &append_lock).await?;

            Ok::<_, anyhow::Error>((status.to_string(), detail))
        });
    }

    while let Some(result) = join_set.join_next().await {
        let (status, _) = result??;
        match status.as_str() {
            "PASS" => summary.verified_pass += 1,
            "FAIL" => summary.verified_fail += 1,
            _ => summary.verified_skip += 1,
        }
    }

    write_json(summary_path, &summary).await?;
    Ok(summary)
}

async fn execute_audit(
    cli: &CliOptions,
    backend: &Backend,
    run_id: &str,
    summary_path: &Path,
    audit_path: &Path,
) -> Result<CommandSummary> {
    let objects = backend.list_objects(cli.prefix.as_deref()).await?;
    let cutoff = cli
        .cutoff_before
        .context("audit command requires cutoff_before")?;
    let remote_staged_keys = objects
        .iter()
        .filter(|object| object.key.ends_with("-thumbnail.png") && !object.key.ends_with("-bak-thumbnail.png"))
        .map(|object| object.key.clone())
        .collect::<std::collections::HashSet<_>>();
    let videos = objects
        .iter()
        .filter(|object| object.key.ends_with(".mp4"))
        .cloned()
        .collect::<Vec<_>>();

    let manifest_path =
        state_dir_for_scope_and_bucket(&cli.manifest_dir, cli.scope, backend.bucket_name())
            .join("manifest.jsonl");
    let manifest_entries = load_manifest_entries(manifest_path.as_path()).await?;

    let completed = manifest_entries
        .iter()
        .filter(|entry| entry.backend == backend.kind() && entry.bucket == backend.bucket_name())
        .filter(|entry| entry.status == ManifestStatus::Completed)
        .count();
    let failed = manifest_entries
        .iter()
        .filter(|entry| entry.backend == backend.kind() && entry.bucket == backend.bucket_name())
        .filter(|entry| entry.status == ManifestStatus::Failed)
        .count();

    let candidates_before_cutoff = videos
        .iter()
        .filter(|object| object.last_modified <= cutoff)
        .filter(|object| {
            cli.prefix
                .as_deref()
                .map(|prefix| object.key.starts_with(prefix))
                .unwrap_or(true)
        })
        .count();

    let remaining_work = videos
        .iter()
        .filter(|v| v.last_modified <= cutoff)
        .filter(|v| {
            cli.prefix
                .as_deref()
                .map(|prefix| v.key.starts_with(prefix))
                .unwrap_or(true)
        })
        .filter_map(|v| staged_thumbnail_key(&v.key))
        .filter(|staged_key| !remote_staged_keys.contains(staged_key))
        .count();

    let report = AuditReport {
        scope: cli.scope.as_str().to_string(),
        bucket: backend.bucket_name().to_string(),
        total_objects: objects.len(),
        total_candidates_before_cutoff: candidates_before_cutoff,
        total_existing_staged: remote_staged_keys.len(),
        manifest_completed: completed,
        manifest_failed: failed,
        remaining_work,
    };
    write_json(audit_path, &report).await?;

    let summary = CommandSummary {
        run_id: run_id.to_string(),
        command: cli.command.as_str().to_string(),
        scope: cli.scope.as_str().to_string(),
        bucket: backend.bucket_name().to_string(),
        cutoff: cutoff.to_rfc3339(),
        execute: false,
        total_objects: report.total_objects,
        candidate_videos: report.total_candidates_before_cutoff,
        existing_staged_objects: report.total_existing_staged,
        planned_process: report.remaining_work,
        completed: report.manifest_completed,
        failed: report.manifest_failed,
        ..Default::default()
    };
    write_json(summary_path, &summary).await?;
    Ok(summary)
}

async fn execute_seed_test_data(
    cli: &CliOptions,
    backend: &Backend,
    run_id: &str,
    summary_path: &Path,
) -> Result<CommandSummary> {
    if !cli.scope.is_test() {
        bail!("seed-test-data is only allowed for test scopes");
    }

    let video_data = create_seed_video_bytes().await?;
    let old_thumbnail = extract_thumbnail_with_seek(&video_data, Some("00:00:01"))
        .await
        .context("generate old-style thumbnail from second frame")?;

    let mut seeded = 0usize;
    for index in 1..=cli.seed_count {
        let video_id = format!("seed-video-{}-{index}", Utc::now().timestamp());
        let publisher = "test-user";
        let video_key = format!("{publisher}/{video_id}.mp4");
        let thumbnail_key = format!("{publisher}/{video_id}_thumbnail.png");

        backend.upload_video(&video_key, video_data.clone()).await?;
        backend
            .upload_png(&thumbnail_key, old_thumbnail.clone())
            .await?;
        seeded += 1;
    }

    let summary = CommandSummary {
        run_id: run_id.to_string(),
        command: cli.command.as_str().to_string(),
        scope: cli.scope.as_str().to_string(),
        bucket: backend.bucket_name().to_string(),
        execute: true,
        seeded,
        ..Default::default()
    };
    write_json(summary_path, &summary).await?;
    Ok(summary)
}

async fn create_seed_video_bytes() -> Result<Vec<u8>> {
    let temp_dir = tempdir().context("create temp dir for seed video")?;
    let video_path = temp_dir.path().join("seed.mp4");

    let output = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=16x16:d=1:r=1",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=16x16:d=1:r=1",
            "-f",
            "lavfi",
            "-i",
            "color=c=green:s=16x16:d=1:r=1",
            "-filter_complex",
            "[0:v][1:v][2:v]concat=n=3:v=1:a=0,format=yuv420p",
            video_path
                .to_str()
                .context("seed video path must be valid utf-8")?,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn ffmpeg for seed video")?
        .wait_with_output()
        .await
        .context("wait for ffmpeg seed video")?;

    if !output.status.success() {
        bail!(
            "ffmpeg failed creating seed video: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::read(video_path)
        .await
        .context("read generated seed video")
}

async fn load_manifest_entries(path: &Path) -> Result<Vec<ManifestEntry>> {
    if !fs::try_exists(path).await.unwrap_or(false) {
        return Ok(Vec::new());
    }

    let content = fs::read(path)
        .await
        .with_context(|| format!("read manifest {}", path.display()))?;
    parse_manifest_entries(&content).with_context(|| format!("parse manifest {}", path.display()))
}

async fn append_jsonl<T: Serialize>(path: &Path, value: &T, lock: &Mutex<()>) -> Result<()> {
    let _guard = lock.lock().await;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("open append file {}", path.display()))?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line).await?;
    file.flush().await?;
    file.sync_data().await?;
    Ok(())
}

async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(value)?;
    fs::write(path, json)
        .await
        .with_context(|| format!("write json file {}", path.display()))
}

fn render_terminal_summary(summary: &CommandSummary) -> String {
    let mut lines = vec![
        format!("Command: {}", summary.command),
        format!("Scope: {}", summary.scope),
        format!("Bucket: {}", summary.bucket),
        format!("Run ID: {}", summary.run_id),
    ];

    match summary.command.as_str() {
        "seed-test-data" => {
            lines.push(format!("Seeded: {}", summary.seeded));
        }
        "run" => {
            let mode = if summary.execute {
                "execute"
            } else {
                "dry-run"
            };
            lines.push(format!("Cutoff: {}", summary.cutoff));
            lines.push(format!("Mode: {mode}"));
            lines.push(format!("Total objects: {}", summary.total_objects));
            lines.push(format!("Candidate videos: {}", summary.candidate_videos));
            lines.push(format!(
                "Existing staged -thumbnail.png: {}",
                summary.existing_staged_objects
            ));
            lines.push(format!("Planned process: {}", summary.planned_process));
            lines.push(format!(
                "Skip remote exists: {}",
                summary.skip_remote_exists
            ));
            lines.push(format!(
                "Skip manifest done: {}",
                summary.skip_manifest_done
            ));
            if summary.execute {
                lines.push(format!("Completed: {}", summary.completed));
                lines.push(format!("Failed: {}", summary.failed));
            } else {
                lines.push(format!("Dry-run candidates: {}", summary.dry_run));
            }
        }
        "verify" => {
            lines.push(format!("Cutoff: {}", summary.cutoff));
            lines.push(format!("Total objects: {}", summary.total_objects));
            lines.push(format!("Candidates checked: {}", summary.candidate_videos));
            lines.push(format!("PASS: {}", summary.verified_pass));
            lines.push(format!("FAIL: {}", summary.verified_fail));
            lines.push(format!("SKIP: {}", summary.verified_skip));
            lines.push("Note: verify only checks videos that already have a staged thumbnail. Run audit to confirm remaining count.".to_string());
        }
        "audit" => {
            lines.push(format!("Cutoff: {}", summary.cutoff));
            lines.push(format!("Total objects: {}", summary.total_objects));
            lines.push(format!(
                "Candidate videos before cutoff: {}",
                summary.candidate_videos
            ));
            lines.push(format!(
                "Existing staged -thumbnail.png: {}",
                summary.existing_staged_objects
            ));
            lines.push(format!("Manifest completed: {}", summary.completed));
            lines.push(format!("Manifest failed: {}", summary.failed));
            lines.push(format!(
                "Remaining videos to backfill: {}",
                summary.planned_process
            ));
        }
        _ => {}
    }

    lines.join("\n")
}

fn state_dir_for_scope_and_bucket(manifest_dir: &str, scope: Scope, bucket: &str) -> PathBuf {
    PathBuf::from(manifest_dir)
        .join(scope.as_str())
        .join(sanitize_path_component(bucket))
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

fn parse_manifest_entries(bytes: &[u8]) -> Result<Vec<ManifestEntry>> {
    let ended_with_newline = bytes.last() == Some(&b'\n');
    let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let mut entries = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }

        let is_last_line = index + 1 == lines.len();
        match serde_json::from_slice::<ManifestEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(err) if is_last_line && !ended_with_newline => {
                tracing::warn!("ignoring torn final manifest line: {err}");
            }
            Err(err) => {
                return Err(err).with_context(|| format!("invalid manifest line {}", index + 1));
            }
        }
    }

    Ok(entries)
}

fn compare_png_pixels(expected_png: &[u8], actual_png: &[u8]) -> Result<bool> {
    let expected = image::load_from_memory_with_format(expected_png, ImageFormat::Png)
        .context("decode expected png")?
        .to_rgba8();
    let actual = image::load_from_memory_with_format(actual_png, ImageFormat::Png)
        .context("decode staged png")?
        .to_rgba8();

    Ok(expected.dimensions() == actual.dimensions() && expected.as_raw() == actual.as_raw())
}

#[cfg(test)]
mod tests {
    use super::{
        compare_png_pixels, load_manifest_entries, render_terminal_summary,
        state_dir_for_scope_and_bucket, CommandSummary,
    };
    use backfill_thumbnails::{BackendKind, ManifestEntry, ManifestStatus, Scope};
    use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
    use std::io::Cursor;
    use tempfile::tempdir;
    use tokio::fs;

    #[test]
    fn state_dir_is_bucket_scoped() {
        let dir =
            state_dir_for_scope_and_bucket("artifacts/backfill", Scope::TestSfwStorj, "bucket-a");
        assert!(dir.ends_with("artifacts/backfill/test-sfw-storj/bucket-a"));
    }

    #[tokio::test]
    async fn load_manifest_entries_ignores_a_torn_last_line() {
        let temp = tempdir().expect("tempdir");
        let manifest_path = temp.path().join("manifest.jsonl");
        let complete = serde_json::to_string(&ManifestEntry {
            backend: BackendKind::Storj,
            bucket: "bucket-a".to_string(),
            source_video_key: "publisher/video-1.mp4".to_string(),
            staged_thumbnail_key: "publisher/video-1-thumbnail.png".to_string(),
            status: ManifestStatus::Completed,
        })
        .expect("serialize manifest entry");
        let torn_line = "{\"backend\":\"Storj\",\"bucket\":\"bucket-a\"";
        fs::write(&manifest_path, format!("{complete}\n{torn_line}"))
            .await
            .expect("write manifest");

        let entries = load_manifest_entries(&manifest_path)
            .await
            .expect("load manifest");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].bucket, "bucket-a");
    }

    #[tokio::test]
    async fn load_manifest_entries_rejects_invalid_complete_lines() {
        let temp = tempdir().expect("tempdir");
        let manifest_path = temp.path().join("manifest.jsonl");
        fs::write(&manifest_path, b"{\"invalid\":true}\n")
            .await
            .expect("write manifest");

        let error = load_manifest_entries(&manifest_path)
            .await
            .expect_err("expected invalid manifest line to fail");

        assert!(format!("{error:#}").contains("manifest"));
    }

    #[test]
    fn compare_png_pixels_accepts_identical_pixels_with_different_encodings() {
        let expected = encode_rgb_png(12, 34, 56);
        let actual = encode_rgba_png(12, 34, 56, 255);

        assert_ne!(expected, actual);
        assert!(compare_png_pixels(&expected, &actual).expect("compare images"));
    }

    #[test]
    fn compare_png_pixels_rejects_different_pixels() {
        let expected = encode_rgb_png(12, 34, 56);
        let actual = encode_rgb_png(99, 88, 77);

        assert!(!compare_png_pixels(&expected, &actual).expect("compare images"));
    }

    #[test]
    fn render_terminal_summary_for_seed_test_data() {
        let summary = CommandSummary {
            run_id: "20260424T040000Z".to_string(),
            command: "seed-test-data".to_string(),
            scope: "test-sfw-storj".to_string(),
            bucket: "test-duplicate".to_string(),
            seeded: 3,
            ..Default::default()
        };

        let rendered = render_terminal_summary(&summary);

        assert!(rendered.contains("Command: seed-test-data"));
        assert!(rendered.contains("Seeded: 3"));
    }

    #[test]
    fn render_terminal_summary_for_run_execute() {
        let summary = CommandSummary {
            run_id: "20260424T040100Z".to_string(),
            command: "run".to_string(),
            scope: "test-sfw-storj".to_string(),
            bucket: "test-duplicate".to_string(),
            execute: true,
            total_objects: 12,
            candidate_videos: 5,
            existing_staged_objects: 1,
            planned_process: 4,
            skip_remote_exists: 1,
            skip_manifest_done: 0,
            completed: 4,
            failed: 0,
            ..Default::default()
        };

        let rendered = render_terminal_summary(&summary);

        assert!(rendered.contains("Mode: execute"));
        assert!(rendered.contains("Completed: 4"));
        assert!(rendered.contains("Failed: 0"));
        assert!(rendered.contains("Existing staged -thumbnail.png: 1"));
    }

    #[test]
    fn render_terminal_summary_for_verify() {
        let summary = CommandSummary {
            run_id: "20260424T040200Z".to_string(),
            command: "verify".to_string(),
            scope: "test-sfw-storj".to_string(),
            bucket: "test-duplicate".to_string(),
            total_objects: 12,
            candidate_videos: 3,
            verified_pass: 2,
            verified_fail: 1,
            verified_skip: 0,
            ..Default::default()
        };

        let rendered = render_terminal_summary(&summary);

        assert!(rendered.contains("Candidates checked: 3"));
        assert!(rendered.contains("PASS: 2"));
        assert!(rendered.contains("FAIL: 1"));
    }

    #[test]
    fn render_terminal_summary_for_audit() {
        let summary = CommandSummary {
            run_id: "20260424T040300Z".to_string(),
            command: "audit".to_string(),
            scope: "test-sfw-storj".to_string(),
            bucket: "test-duplicate".to_string(),
            total_objects: 12,
            candidate_videos: 5,
            existing_staged_objects: 1,
            planned_process: 4,
            completed: 1,
            failed: 0,
            ..Default::default()
        };

        let rendered = render_terminal_summary(&summary);

        assert!(rendered.contains("Candidate videos before cutoff: 5"));
        assert!(rendered.contains("Existing staged -thumbnail.png: 1"));
        assert!(rendered.contains("Manifest completed: 1"));
        assert!(rendered.contains("Remaining videos to backfill: 4"));
    }

    fn encode_rgb_png(red: u8, green: u8, blue: u8) -> Vec<u8> {
        let image =
            DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, image::Rgb([red, green, blue])));
        encode_png(image)
    }

    fn encode_rgba_png(red: u8, green: u8, blue: u8, alpha: u8) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([red, green, blue, alpha]),
        ));
        encode_png(image)
    }

    fn encode_png(image: DynamicImage) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        image
            .write_to(&mut buffer, ImageFormat::Png)
            .expect("encode png");
        buffer.into_inner()
    }
}

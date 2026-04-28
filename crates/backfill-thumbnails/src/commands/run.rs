use super::{append_jsonl, load_manifest_entries, write_json, CommandSummary};
use anyhow::{Context, Result};
use backfill_thumbnails::{
    build_run_plan, CliOptions, ManifestEntry, ManifestIndex, ManifestStatus, PlannedAction,
};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use storj_interface::thumbnail::extract_thumbnail_with_seek;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

use crate::backend::Backend;

#[derive(Debug, Serialize)]
struct FailureRecord {
    video_key: String,
    staged_thumbnail_key: String,
    error: String,
}

pub async fn execute_run(
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
        .filter(|object| {
            object.key.ends_with("-thumbnail.png") && !object.key.ends_with("-bak-thumbnail.png")
        })
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
                    tracing::error!(
                        scope = ?backend.kind(),
                        bucket = %backend.bucket_name(),
                        video_key = %video_key,
                        staged_thumbnail_key = %staged_thumbnail_key,
                        error = %err,
                        "thumbnail backfill failed"
                    );
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

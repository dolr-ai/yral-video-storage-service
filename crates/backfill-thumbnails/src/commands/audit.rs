use super::{load_manifest_entries, state_dir_for_scope_and_bucket, write_json, CommandSummary};
use anyhow::{Context, Result};
use backfill_thumbnails::{staged_thumbnail_key, CliOptions, ManifestStatus};
use serde::Serialize;
use std::path::Path;

use crate::backend::Backend;

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

pub async fn execute_audit(
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

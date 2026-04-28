mod backend;
mod commands;
mod telemetry;

use anyhow::{bail, Context, Result};
use backfill_thumbnails::{parse_cli_args, CommandKind};
use chrono::Utc;
use commands::{
    execute_audit, execute_run, execute_seed_test_data, execute_verify,
    state_dir_for_scope_and_bucket, CommandSummary,
};
use std::env;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;

use crate::backend::Backend;

#[tokio::main]
async fn main() -> Result<()> {
    let _sentry_guard = telemetry::init_sentry();
    telemetry::init_tracing();
    telemetry::spawn_sigterm_flush();

    let result = run().await;
    if let Err(ref e) = result {
        tracing::error!(error = format!("{e:#}"), "backfill-thumbnails fatal error");
        telemetry::flush_sentry();
    }
    result
}

async fn run() -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::{render_terminal_summary, CommandSummary};

    #[test]
    fn render_terminal_summary_for_seed_test_data() {
        let summary = CommandSummary {
            run_id: "20260424T040000Z".to_string(),
            command: "seed-test-data".to_string(),
            scope: "storj".to_string(),
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
            scope: "storj".to_string(),
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
            scope: "storj".to_string(),
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
            scope: "storj".to_string(),
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
}

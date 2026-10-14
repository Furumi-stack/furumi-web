use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::scheduler::{Job, JobContext, JobLog, PendingReview};

/// Guard to prevent overlapping inbox_discover runs.
static DISCOVER_RUNNING: AtomicBool = AtomicBool::new(false);

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "aac", "m4a", "wav", "ape", "wv", "wma", "tta", "aiff", "aif",
];

/// How long a `failed` review must stay untouched before discover
/// automatically requeues it (instead of creating a new row per attempt).
const FAILED_RETRY_COOLDOWN_SECS: i64 = 3600;

/// Leftover files that are safe to purge from inbox folders that no longer
/// contain any audio (covers, playlists, rip logs and similar sidecar files).
const JUNK_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "m3u", "m3u8", "cue", "log", "txt", "nfo", "sfv",
    "md5", "accurip", "url", "ini", "pdf",
];
const JUNK_FILENAMES: &[&str] = &[".ds_store", "thumbs.db", "desktop.ini"];

/// Junk younger than this is kept — an upload might still be in progress.
const JUNK_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

pub struct InboxDiscoverJob;

#[async_trait::async_trait]
impl Job for InboxDiscoverJob {
    fn name(&self) -> &'static str {
        "inbox_discover"
    }

    fn description(&self) -> &'static str {
        "Scan inbox for new audio files and queue them for processing"
    }

    fn default_cron(&self) -> &'static str {
        "0 */5 * * * *"
    }

    async fn run(&self, ctx: &JobContext, log: &mut JobLog) -> anyhow::Result<()> {
        let run_start = std::time::Instant::now();
        let run_outcome = "completed";
        // Prevent overlapping discover runs
        if DISCOVER_RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            log.info("Another inbox_discover is already running, skipping");
            return Ok(());
        }
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                DISCOVER_RUNNING.store(false, Ordering::SeqCst);
            }
        }
        let _guard = Guard;
        struct MetricsGuard {
            start: std::time::Instant,
            outcome: &'static str,
        }
        impl Drop for MetricsGuard {
            fn drop(&mut self) {
                crate::metrics::record_agent_discover_run(self.outcome, self.start.elapsed());
            }
        }
        let mut metrics_guard = MetricsGuard {
            start: run_start,
            outcome: run_outcome,
        };

        let config = &ctx.config;

        if config.agent_inbox_dir.is_empty() {
            log.info("No inbox directory configured, skipping");
            return Ok(());
        }

        let inbox = Path::new(&config.agent_inbox_dir);
        if !inbox.exists() {
            log.warn(&format!("Inbox path does not exist: {}", inbox.display()));
            return Ok(());
        }

        let mut audio_files = Vec::new();
        collect_audio_files(inbox, &mut audio_files).await?;

        // Purge leftover junk (covers, playlists, logs) from subtrees that no
        // longer contain audio, so processed uploads don't linger forever.
        cleanup_inbox_junk(inbox, JUNK_MIN_AGE).await;

        log.info(&format!("Found {} audio files in inbox", audio_files.len()));
        if audio_files.is_empty() {
            return Ok(());
        }

        let groups = group_by_folder(&audio_files);
        log.info(&format!("Grouped into {} folder batches", groups.len()));

        let mut discovered = 0u64;
        let mut skipped_hash = 0u64;
        let mut skipped_existing = 0u64;
        let mut requeued = 0u64;

        for (_folder, files) in &groups {
            for file_path in files {
                let input_path_str =
                    crate::media_paths::path_for_root(&config.agent_inbox_dir, file_path)
                        .unwrap_or_else(|| file_path.to_string_lossy().to_string());

                // One review row per path: any existing row blocks creating a
                // new one. A stale "failed" row is requeued in place instead,
                // so retries don't multiply rows. "rejected" stays rejected.
                match PendingReview::latest_for_path(&ctx.pool, &input_path_str).await {
                    Ok(None) => {}
                    Ok(Some((id, status, updated_at))) => {
                        if status == "failed" {
                            let stale = chrono::DateTime::parse_from_rfc3339(&updated_at)
                                .map(|t| {
                                    chrono::Utc::now().signed_duration_since(t).num_seconds()
                                        >= FAILED_RETRY_COOLDOWN_SECS
                                })
                                .unwrap_or(true);
                            if stale {
                                match PendingReview::requeue_by_ids(&ctx.db, &[id]).await {
                                    Ok(()) => requeued += 1,
                                    Err(e) => log.warn(&format!(
                                        "Failed to requeue review {id} for {input_path_str}: {e}"
                                    )),
                                }
                            } else {
                                skipped_existing += 1;
                            }
                        } else {
                            skipped_existing += 1;
                        }
                        continue;
                    }
                    Err(e) => {
                        log.warn(&format!(
                            "Error checking existing review for {}: {e}",
                            input_path_str
                        ));
                        continue;
                    }
                }

                // Compute SHA-256 hash
                let path_clone = file_path.to_path_buf();
                let hash_start = std::time::Instant::now();
                let (hash, file_size) =
                    match tokio::task::spawn_blocking(move || -> anyhow::Result<(String, i64)> {
                        let data = std::fs::read(&path_clone)?;
                        let digest = Sha256::digest(&data);
                        let hash = format!("{:x}", digest);
                        let size = data.len() as i64;
                        Ok((hash, size))
                    })
                    .await?
                    {
                        Ok(v) => {
                            crate::metrics::record_agent_file_hash(hash_start.elapsed(), v.1, "ok");
                            v
                        }
                        Err(e) => {
                            crate::metrics::record_agent_file_hash(
                                hash_start.elapsed(),
                                0,
                                "error",
                            );
                            log.warn(&format!("Failed to hash {}: {e}", file_path.display()));
                            continue;
                        }
                    };

                // Skip if hash already in media_files
                if crate::agent::rag::file_hash_exists_verified(
                    &ctx.pool,
                    &hash,
                    &config.agent_storage_dir,
                )
                .await
                .unwrap_or(false)
                {
                    // Close the upload by removing the exact inbox duplicate;
                    // otherwise its history remains queued forever.
                    tokio::fs::remove_file(file_path).await?;
                    skipped_hash += 1;
                    continue;
                }

                // Extract raw metadata
                let path_for_meta = file_path.to_path_buf();
                let metadata_start = std::time::Instant::now();
                let raw_meta = match tokio::task::spawn_blocking(move || {
                    crate::agent::metadata::extract(&path_for_meta)
                })
                .await?
                {
                    Ok(m) => {
                        crate::metrics::record_agent_metadata(metadata_start.elapsed(), "ok");
                        m
                    }
                    Err(e) => {
                        crate::metrics::record_agent_metadata(metadata_start.elapsed(), "error");
                        log.warn(&format!(
                            "Failed to extract metadata from {}: {e}",
                            file_path.display()
                        ));
                        continue;
                    }
                };

                // Parse path hints
                let relative = file_path.strip_prefix(inbox).unwrap_or(file_path);
                let uploader = crate::jobs::uploader_from_relative_path(&ctx.pool, relative).await;
                let hinted_relative = crate::jobs::strip_user_upload_prefix(relative);
                let hints = crate::agent::path_hints::parse(&hinted_relative);

                // Build context JSON
                let context = serde_json::json!({
                    "sha256": hash,
                    "file_size": file_size,
                    "raw_title": raw_meta.title,
                    "raw_artist": raw_meta.artist,
                    "raw_album": raw_meta.album,
                    "raw_track_number": raw_meta.track_number,
                    "raw_year": raw_meta.year,
                    "raw_genre": raw_meta.genre,
                    "duration_secs": raw_meta.duration_secs,
                    "audio_bitrate": raw_meta.audio_bitrate,
                    "audio_sample_rate": raw_meta.audio_sample_rate,
                    "audio_bit_depth": raw_meta.audio_bit_depth,
                    "uploaded_by_user_id": uploader.user_id,
                    "uploader_name": uploader.name,
                    "path_title": hints.title,
                    "path_artist": hints.artist,
                    "path_album": hints.album,
                    "path_year": hints.year,
                    "path_track_number": hints.track_number,
                });
                let context_str = serde_json::to_string(&context).unwrap_or_default();

                // Create PendingReview with status "queued"
                PendingReview::create_queued(
                    &ctx.db,
                    ctx.run_id,
                    "new_file",
                    Some(&input_path_str),
                    Some(&context_str),
                )
                .await
                .map_err(|e| anyhow::anyhow!("failed to create queued review: {e}"))?;

                discovered += 1;
            }
        }

        log.info(&format!(
            "Discovered {} new files, requeued {} failed, skipped {} (hash known), skipped {} (already tracked)",
            discovered, requeued, skipped_hash, skipped_existing
        ));
        crate::metrics::record_agent_discover_files(
            audio_files.len() as u64,
            discovered,
            skipped_hash,
            skipped_existing,
        );

        // Trigger inbox_process in background if new files were discovered
        // and no orchestrator is already running
        if discovered + requeued > 0 {
            if crate::jobs::inbox_process::is_orchestrator_running() {
                log.info(
                    "New files discovered but inbox_process already running, it will pick them up",
                );
            } else {
                log.info("Spawning inbox_process in background...");
                let config = ctx.config.clone();
                let db = ctx.db.clone();
                let pool = ctx.pool.clone();
                let registry = ctx.registry.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::scheduler::trigger_job_now(
                        &config,
                        &db,
                        &pool,
                        &registry,
                        "inbox_process",
                    )
                    .await
                    {
                        tracing::error!("Background inbox_process trigger failed: {e}");
                    }
                });
            }
        }

        metrics_guard.outcome = run_outcome;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers (moved from inbox_scan.rs)
// ---------------------------------------------------------------------------

pub fn group_by_folder(files: &[PathBuf]) -> Vec<(PathBuf, Vec<PathBuf>)> {
    use std::collections::HashMap;
    let mut map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for f in files {
        let folder = f.parent().unwrap_or(f).to_path_buf();
        map.entry(folder).or_default().push(f.clone());
    }
    let mut groups: Vec<(PathBuf, Vec<PathBuf>)> = map.into_iter().collect();
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, files) in &mut groups {
        files.sort();
    }
    groups
}

pub async fn collect_audio_files(dir: &Path, audio: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if crate::local_uploads::incomplete_batch(dir).await? {
        return Ok(());
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            Box::pin(collect_audio_files(&entry.path(), audio)).await?;
        } else if ft.is_file() && is_audio_file(&name) {
            audio.push(entry.path());
        }
    }
    Ok(())
}

pub fn is_audio_file(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    AUDIO_EXTENSIONS.contains(&ext.as_str())
}

fn is_junk_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    // macOS AppleDouble sidecars ("._track.mp3") and well-known junk names
    if lower.starts_with("._") || JUNK_FILENAMES.contains(&lower.as_str()) {
        return true;
    }
    let ext = lower.rsplit('.').next().unwrap_or("");
    JUNK_EXTENSIONS.contains(&ext)
}

/// Remove leftover sidecar files (covers, playlists, rip logs) from inbox
/// subtrees that no longer contain any audio, then prune emptied directories.
///
/// Junk younger than `min_age` is kept in case an upload is still in
/// progress, and unknown file types are never touched. Returns `true` when
/// `dir` still contains something worth keeping (so the caller must not
/// remove it).
async fn cleanup_inbox_junk(dir: &Path, min_age: std::time::Duration) -> bool {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return true,
    };

    let mut has_audio = false;
    let mut keep_other = false;
    let mut junk: Vec<PathBuf> = Vec::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = match entry.file_type().await {
            Ok(ft) => ft,
            Err(_) => {
                keep_other = true;
                continue;
            }
        };
        if ft.is_dir() {
            if Box::pin(cleanup_inbox_junk(&entry.path(), min_age)).await {
                keep_other = true;
            } else {
                let _ = tokio::fs::remove_dir(&entry.path()).await;
            }
        } else if !name.starts_with('.') && is_audio_file(&name) {
            // dotfiles are invisible to discovery, so they don't count as audio
            has_audio = true;
        } else if is_junk_file(&name) {
            junk.push(entry.path());
        } else {
            keep_other = true;
        }
    }

    if has_audio {
        return true;
    }

    let mut junk_left = false;
    for path in junk {
        let old_enough = tokio::fs::metadata(&path)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= min_age);
        if !old_enough || tokio::fs::remove_file(&path).await.is_err() {
            junk_left = true;
        }
    }

    keep_other || junk_left
}

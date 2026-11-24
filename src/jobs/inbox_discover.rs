use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use super::inbox_review::{
    finish_exact_review, reconcile_open_reviews, same_file_version, tracked_files,
};
pub use super::inbox_review::{link_exact_import, remove_exact_inbox_copy};

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

        if let Err(error) = crate::import_playlists::reconcile(&ctx.pool, None).await {
            log.warn(&format!("Could not reconcile import playlists: {error}"));
        }
        let config = &ctx.config;

        if config.agent_inbox_dir.is_empty() {
            log.info("No inbox directory configured, skipping");
            return Ok(());
        }

        reconcile_open_reviews(&ctx.pool, config, log).await?;
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
            INBOX_HASHES.lock().unwrap().clear();
            return Ok(());
        }

        let relative_paths: Vec<_> = audio_files
            .iter()
            .filter_map(|path| crate::media_paths::path_for_root(&config.agent_inbox_dir, path))
            .collect();
        let tracked: HashMap<_, _> = tracked_files(&ctx.pool, &relative_paths)
            .await?
            .into_iter()
            .map(|r| (r.input_path.clone(), r))
            .collect();
        let dismissed: HashSet<(String, String)> = sqlx::query_as(
            "SELECT input_path, sha256 FROM furumusic__dismissed_review WHERE input_path = ANY($1)",
        )
        .bind(&relative_paths)
        .fetch_all(&ctx.pool)
        .await?
        .into_iter()
        .collect();
        let present: HashSet<&PathBuf> = audio_files.iter().collect();
        INBOX_HASHES
            .lock()
            .unwrap()
            .retain(|path, _| present.contains(path));
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

                let existing = tracked.get(&input_path_str);
                if existing.is_some_and(|r| r.status == "processing") {
                    skipped_existing += 1;
                    continue;
                }
                let (hash, file_size) = match observed_inbox_hash(file_path).await {
                    Ok(value) => value,
                    Err(error) => {
                        log.warn(&format!(
                            "Could not inspect {}: {error}",
                            file_path.display()
                        ));
                        continue;
                    }
                };
                if dismissed.contains(&(input_path_str.clone(), hash.clone())) {
                    skipped_existing += 1;
                    continue;
                }
                let same = existing.filter(|r| r.sha256.as_deref() == Some(hash.as_str()));
                if let Some(review) = same
                    && !matches!(review.status.as_str(), "approved" | "auto_approved")
                {
                    if review.status == "failed" {
                        let stale = chrono::DateTime::parse_from_rfc3339(&review.updated_at)
                            .map(|t| {
                                chrono::Utc::now().signed_duration_since(t).num_seconds()
                                    >= FAILED_RETRY_COOLDOWN_SECS
                            })
                            .unwrap_or(true);
                        if stale {
                            requeued += sqlx::query("UPDATE furumusic__pending_review SET status='queued',
                                error_message=NULL, updated_at=$3 WHERE id=$1 AND status='failed'
                                AND updated_at=$2 AND strpos(COALESCE(error_message,''), 'Duplicate check needs review: ') = 0")
                                .bind(review.id).bind(&review.updated_at)
                                .bind(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                                .execute(&ctx.pool).await?.rows_affected();
                        }
                    }
                    skipped_existing += 1;
                    continue;
                }

                let exact = match crate::agent::rag::verified_audio_media(
                    &ctx.pool,
                    &hash,
                    &config.agent_storage_dir,
                )
                .await
                {
                    Ok(media) => media,
                    Err(error) => {
                        log.warn(&format!(
                            "Could not verify catalog audio for {input_path_str}: {error}"
                        ));
                        continue;
                    }
                };
                if let (Some(review), Some(media)) = (same, exact) {
                    match finish_exact_review(&ctx.pool, config, review, &hash, media).await {
                        Ok(true) => skipped_hash += 1,
                        Ok(false) => skipped_existing += 1,
                        Err(error) => log.warn(&format!(
                            "Could not reconcile review {}: {error:#}",
                            review.id
                        )),
                    }
                    continue;
                }

                // Extract raw metadata
                let path_for_meta = file_path.to_path_buf();
                let metadata_start = std::time::Instant::now();
                let raw_meta = if exact.is_some() {
                    Default::default()
                } else {
                    match tokio::task::spawn_blocking(move || {
                        crate::agent::metadata::extract(&path_for_meta)
                    })
                    .await?
                    {
                        Ok(m) => {
                            crate::metrics::record_agent_metadata(metadata_start.elapsed(), "ok");
                            m
                        }
                        Err(e) => {
                            crate::metrics::record_agent_metadata(
                                metadata_start.elapsed(),
                                "error",
                            );
                            log.warn(&format!(
                                "Failed to extract metadata from {}: {e}",
                                file_path.display()
                            ));
                            continue;
                        }
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

                if PendingReview::enqueue_discovered(
                    &ctx.pool,
                    ctx.run_id,
                    &input_path_str,
                    &hash,
                    &context_str,
                )
                .await?
                {
                    if let Some(media) = exact
                        && let Some(review) =
                            tracked_files(&ctx.pool, std::slice::from_ref(&input_path_str))
                                .await?
                                .pop()
                        && review.status == "queued"
                        && review.sha256.as_deref() == Some(hash.as_str())
                    {
                        match finish_exact_review(&ctx.pool, config, &review, &hash, media).await {
                            Ok(true) => {
                                skipped_hash += 1;
                                continue;
                            }
                            Ok(false) => {}
                            Err(error) => log.warn(&format!(
                                "Could not reconcile review {}: {error:#}",
                                review.id
                            )),
                        }
                    }
                    discovered += 1;
                }
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

// Inbox files awaiting a human decision can be large. Rehash them only when
// their filesystem identity or timestamps change. Destructive operations
// always hash the live source again.
static INBOX_HASHES: LazyLock<Mutex<HashMap<PathBuf, (std::fs::Metadata, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn observed_inbox_hash(path: &Path) -> anyhow::Result<(String, i64)> {
    let before = tokio::fs::metadata(path).await?;
    {
        let hashes = INBOX_HASHES.lock().unwrap();
        if let Some((version, sha256)) = hashes.get(path)
            && same_file_version(version, &before)
        {
            return Ok((sha256.clone(), i64::try_from(before.len())?));
        }
    }
    let start = std::time::Instant::now();
    let sha256 = crate::agent::mover::sha256_file(path).await?;
    let after = tokio::fs::metadata(path).await?;
    anyhow::ensure!(
        same_file_version(&before, &after),
        "file changed during discovery"
    );
    let size = i64::try_from(after.len())?;
    INBOX_HASHES
        .lock()
        .unwrap()
        .insert(path.to_owned(), (after, sha256.clone()));
    crate::metrics::record_agent_file_hash(start.elapsed(), size, "ok");
    Ok((sha256, size))
}

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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cot::db::{Database, Model};

/// Guard to prevent multiple inbox_process orchestrators from running simultaneously.
static ORCHESTRATOR_RUNNING: AtomicBool = AtomicBool::new(false);

/// Well-known advisory lock ID for the inbox_process orchestrator.
/// PostgreSQL advisory locks use a 64-bit key; this is an arbitrary unique value.
const ORCHESTRATOR_ADVISORY_LOCK_ID: i64 = 0x4655_5255_4D55_5349; // "FURUMUSI" in hex

/// Maximum number of files sent to the LLM in a single batch call.
/// Folders with more files are processed in chunks of this size, otherwise
/// the model's completion window overflows and the JSON response is cut off.
const MAX_LLM_BATCH_FILES: usize = 20;

/// Check if an orchestrator is currently running (used by inbox_discover to avoid redundant triggers).
pub fn is_orchestrator_running() -> bool {
    ORCHESTRATOR_RUNNING.load(Ordering::SeqCst)
}

struct OrchestratorAdvisoryGuard {
    conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>>,
}

impl Drop for OrchestratorAdvisoryGuard {
    fn drop(&mut self) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };

        tokio::spawn(async move {
            match sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
                .bind(ORCHESTRATOR_ADVISORY_LOCK_ID)
                .fetch_one(&mut *conn)
                .await
            {
                Ok(true) => tracing::info!("inbox_process: advisory lock released"),
                Ok(false) => tracing::warn!(
                    "inbox_process: advisory lock was not held by the guard connection"
                ),
                Err(e) => tracing::error!("inbox_process: failed to release advisory lock: {e}"),
            }
        });
    }
}

async fn connection_holds_orchestrator_lock(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
) -> anyhow::Result<bool> {
    let holds_lock = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_locks
            WHERE locktype = 'advisory'
              AND pid = pg_backend_pid()
              AND mode = 'ExclusiveLock'
              AND granted
              AND objsubid = 1
              AND classid::bigint = (($1::bigint >> 32) & 4294967295::bigint)
              AND objid::bigint = ($1::bigint & 4294967295::bigint)
        )
        "#,
    )
    .bind(ORCHESTRATOR_ADVISORY_LOCK_ID)
    .fetch_one(&mut **conn)
    .await?;

    Ok(holds_lock)
}

/// Try to acquire the PostgreSQL advisory lock for the orchestrator.
///
/// The guard owns the same pooled connection that acquired the session-level
/// lock. This matters because PostgreSQL session advisory locks must be
/// released on the same connection, not just through the same pool.
async fn try_acquire_orchestrator_lock(
    pool: &sqlx::PgPool,
) -> anyhow::Result<Option<OrchestratorAdvisoryGuard>> {
    let mut conn = pool.acquire().await?;

    // Older versions acquired the lock through the pool and could return a
    // locked idle connection. If we get such a connection, drain that stale
    // re-entrant lock before taking the new guarded lock.
    let mut cleaned_stale_locks = 0u8;
    while connection_holds_orchestrator_lock(&mut conn).await? {
        let unlocked = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(ORCHESTRATOR_ADVISORY_LOCK_ID)
            .fetch_one(&mut *conn)
            .await?;

        cleaned_stale_locks = cleaned_stale_locks.saturating_add(u8::from(unlocked));
        if cleaned_stale_locks >= 8 {
            break;
        }
    }
    if cleaned_stale_locks > 0 {
        tracing::warn!(
            count = cleaned_stale_locks,
            "inbox_process: released stale advisory lock(s) from an idle pooled connection"
        );
    }

    let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
        .bind(ORCHESTRATOR_ADVISORY_LOCK_ID)
        .fetch_one(&mut *conn)
        .await?;

    if acquired {
        Ok(Some(OrchestratorAdvisoryGuard { conn: Some(conn) }))
    } else {
        Ok(None)
    }
}

use crate::agent::dto::{FolderContext, NormalizedFields, PathHints, RawMetadata};
use crate::agent::mover;
use crate::agent::normalize::BatchFileInput;
use crate::config::AppConfig;
use crate::music::{Artist, MediaFile, Release, ReleaseArtist, Track, TrackArtist};
use crate::scheduler::{Job, JobContext, JobLog, JobRun, PendingReview, ProcessingStats};

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "aac", "m4a", "wav", "ape", "wv", "wma", "tta", "aiff", "aif",
];

// ---------------------------------------------------------------------------
// InboxProcessJob — orchestrator that runs until ALL queued files are done
// ---------------------------------------------------------------------------

pub struct InboxProcessJob;

#[async_trait::async_trait]
impl Job for InboxProcessJob {
    fn name(&self) -> &'static str {
        "inbox_process"
    }

    fn description(&self) -> &'static str {
        "Orchestrator: process queued files in folder batches"
    }

    fn default_cron(&self) -> &'static str {
        "30 */5 * * * *"
    }

    async fn run(&self, ctx: &JobContext, log: &mut JobLog) -> anyhow::Result<()> {
        // --- Guard 1: AtomicBool (fast in-process check) ---
        let prev = ORCHESTRATOR_RUNNING.load(Ordering::SeqCst);
        tracing::info!(
            previous_value = prev,
            "inbox_process: checking ORCHESTRATOR_RUNNING AtomicBool"
        );
        if ORCHESTRATOR_RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            log.warn(
                "Another inbox_process orchestrator is already running (AtomicBool), skipping",
            );
            anyhow::bail!("another inbox_process orchestrator is already running in this process");
        }
        struct AtomicGuard;
        impl Drop for AtomicGuard {
            fn drop(&mut self) {
                tracing::info!("inbox_process: releasing ORCHESTRATOR_RUNNING AtomicBool");
                ORCHESTRATOR_RUNNING.store(false, Ordering::SeqCst);
            }
        }
        let _atomic_guard = AtomicGuard;

        // --- Guard 2: PostgreSQL advisory lock (cross-process/binary safe) ---
        let _advisory_guard = match try_acquire_orchestrator_lock(&ctx.pool).await? {
            Some(guard) => guard,
            None => {
                log.warn("Another inbox_process orchestrator holds the advisory lock");
                anyhow::bail!(
                    "inbox_process advisory lock is held by another database session; no in-process orchestrator is running"
                );
            }
        };
        tracing::info!("inbox_process: advisory lock acquired");

        let config = Arc::clone(&ctx.config);
        let mut total_ok = 0u64;
        let mut total_fail = 0u64;

        // Outer loop: re-check for newly queued files after each round
        loop {
            let queued = PendingReview::list_queued(&ctx.db)
                .await
                .map_err(|e| anyhow::anyhow!("failed to list queued reviews: {e}"))?;

            if queued.is_empty() {
                if total_ok == 0 && total_fail == 0 {
                    log.info("No queued files to process");
                } else {
                    log.info("No more queued files, finishing");
                }
                break;
            }

            // Group queued reviews by parent folder
            let groups = group_reviews_by_folder(&queued, &config.agent_inbox_dir);
            log.info(&format!(
                "{} queued file(s) in {} folder batch(es)",
                queued.len(),
                groups.len(),
            ));

            for (folder_rel, reviews) in groups {
                let file_count = reviews.len();
                log.info(&format!(
                    "Folder batch: \"{}\" ({} files)",
                    folder_rel, file_count,
                ));

                // Large folders are split into chunks: a single LLM call for
                // 100+ files overflows the completion window and the whole
                // batch fails with a truncated-JSON parse error.
                for chunk in reviews.chunks(MAX_LLM_BATCH_FILES) {
                    let (ok, fail) = process_folder_batch(
                        &ctx.db,
                        &config,
                        &ctx.pool,
                        &folder_rel,
                        chunk.to_vec(),
                        log,
                    )
                    .await;

                    total_ok += ok;
                    total_fail += fail;
                }
                log.info(&format!(
                    "Folder done. Total so far: {total_ok} ok, {total_fail} err"
                ));
            }
        }

        // Cleanup empty dirs
        let inbox_path = Path::new(&config.agent_inbox_dir);
        if total_ok > 0 && !config.agent_inbox_dir.is_empty() {
            cleanup_empty_dirs(inbox_path).await;
        }

        log.info(&format!(
            "Orchestrator finished: {total_ok} succeeded, {total_fail} failed"
        ));

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FileProcessJob — registered for admin UI visibility (no cron, never auto-triggered)
// ---------------------------------------------------------------------------

pub struct FileProcessJob;

#[async_trait::async_trait]
impl Job for FileProcessJob {
    fn name(&self) -> &'static str {
        "file_process"
    }

    fn description(&self) -> &'static str {
        "Process audio files through LLM (spawned by orchestrator)"
    }

    fn default_cron(&self) -> &'static str {
        "" // no cron — only spawned by the orchestrator
    }

    async fn run(&self, _ctx: &JobContext, _log: &mut JobLog) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Prepared file — metadata extracted, ready for LLM
// ---------------------------------------------------------------------------

struct PreparedFile {
    review: PendingReview,
    filename: String,
    raw_meta: RawMetadata,
    hints: PathHints,
    context: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Group reviews by parent folder
// ---------------------------------------------------------------------------

fn group_reviews_by_folder(
    reviews: &[PendingReview],
    inbox_dir: &str,
) -> Vec<(String, Vec<PendingReview>)> {
    let mut map: HashMap<String, Vec<PendingReview>> = HashMap::new();

    for r in reviews {
        let path = crate::media_paths::resolve_path_from_root(inbox_dir, r.input_path_str());
        let folder = path.parent().unwrap_or(path.as_path());
        let key = crate::media_paths::path_for_root(inbox_dir, folder)
            .unwrap_or_else(|| folder.to_string_lossy().to_string());
        map.entry(key).or_default().push(r.clone());
    }

    let mut groups: Vec<(String, Vec<PendingReview>)> = map.into_iter().collect();
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    // Sort files within each group by path
    for (_, reviews) in &mut groups {
        reviews.sort_by(|a, b| a.input_path_str().cmp(b.input_path_str()));
    }
    groups
}

// ---------------------------------------------------------------------------
// Process one folder batch
// ---------------------------------------------------------------------------

async fn process_folder_batch(
    db: &Database,
    config: &AppConfig,
    pool: &sqlx::PgPool,
    folder_rel: &str,
    reviews: Vec<PendingReview>,
    orch_log: &mut JobLog,
) -> (u64, u64) {
    let inbox_path = Path::new(&config.agent_inbox_dir);
    let file_count = reviews.len();

    // Create a single JobRun for the folder batch
    let trigger_label = if folder_rel.is_empty() {
        format!("batch({})", file_count)
    } else {
        let short = truncate_path(folder_rel, 20);
        truncate_utf8_bytes(&format!("{short}({})", file_count), 32)
    };
    let mut run = match JobRun::create_running(db, "file_process", &trigger_label).await {
        Ok(r) => r,
        Err(e) => {
            orch_log.error(&format!("Failed to create batch JobRun: {e}"));
            return (0, file_count as u64);
        }
    };
    let batch_start = std::time::Instant::now();
    let mut log = JobLog::with_live_flush(pool.clone(), run.id_val());

    log.info(&format!(
        "Folder batch: \"{folder_rel}\" — {file_count} file(s)"
    ));

    // Phase 1: Prepare all files (extract metadata, parse hints)
    log.info("Phase 1: extracting metadata...");
    let mut prepared: Vec<PreparedFile> = Vec::with_capacity(file_count);
    let mut failed_reviews: Vec<PendingReview> = Vec::new();
    let mut merged_count = 0u64;

    for mut review in reviews {
        let stored_input_path = review.input_path_str().to_owned();
        let file_path =
            crate::media_paths::resolve_path_from_root(&config.agent_inbox_dir, &stored_input_path);
        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_owned();

        // Parse context_json
        let mut context: serde_json::Value = review
            .context_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        // Resolve duplicates and missing sources before any expensive work.
        let sha = context
            .get("sha256")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let file_exists = file_path.exists();
        if !sha.is_empty()
            && crate::agent::rag::file_hash_exists(pool, &sha)
                .await
                .unwrap_or(false)
        {
            // Identical content is already in the library — drop the inbox
            // copy (same as mover::Merged) and close the review.
            if file_exists {
                let _ = tokio::fs::remove_file(&file_path).await;
            }
            let _ = PendingReview::delete_by_ids(db, &[review.id_val()]).await;
            log.info(&format!(
                "{filename}: content already in library (sha256 match) — merged duplicate"
            ));
            crate::metrics::record_agent_file_processed("ok", "merged_duplicate");
            merged_count += 1;
            continue;
        }
        if !file_exists {
            let msg = format!("{filename}: source file missing: {stored_input_path}");
            log.error(&msg);
            let _ = review.set_failed(db, &msg).await;
            failed_reviews.push(review);
            continue;
        }

        // Set status → processing
        let _ = review.set_processing(db).await;

        // Extract metadata (with 60s timeout)
        let path_for_meta = file_path.to_path_buf();
        let metadata_start = std::time::Instant::now();
        let meta_future =
            tokio::task::spawn_blocking(move || crate::agent::metadata::extract(&path_for_meta));
        let raw_meta =
            match tokio::time::timeout(std::time::Duration::from_secs(60), meta_future).await {
                Ok(Ok(Ok(m))) => {
                    crate::metrics::record_agent_metadata(metadata_start.elapsed(), "ok");
                    m
                }
                Ok(Ok(Err(e))) => {
                    crate::metrics::record_agent_metadata(metadata_start.elapsed(), "error");
                    crate::metrics::record_agent_failed("metadata");
                    let msg = format!("{filename}: metadata error: {e}");
                    log.error(&msg);
                    let _ = review.set_failed(db, &msg).await;
                    failed_reviews.push(review);
                    continue;
                }
                Ok(Err(e)) => {
                    crate::metrics::record_agent_metadata(metadata_start.elapsed(), "panic");
                    crate::metrics::record_agent_failed("metadata");
                    let msg = format!("{filename}: metadata panic: {e}");
                    log.error(&msg);
                    let _ = review.set_failed(db, &msg).await;
                    failed_reviews.push(review);
                    continue;
                }
                Err(_) => {
                    crate::metrics::record_agent_metadata(metadata_start.elapsed(), "timeout");
                    crate::metrics::record_agent_failed("metadata");
                    let msg = format!("{filename}: metadata timeout (60s)");
                    log.error(&msg);
                    let _ = review.set_failed(db, &msg).await;
                    failed_reviews.push(review);
                    continue;
                }
            };

        // Parse path hints
        let relative = file_path.strip_prefix(inbox_path).unwrap_or(&file_path);
        let uploader = crate::jobs::uploader_from_relative_path(pool, relative).await;
        let hinted_relative = crate::jobs::strip_user_upload_prefix(relative);
        let hints = crate::agent::path_hints::parse(&hinted_relative);
        if let Some(context_obj) = context.as_object_mut() {
            context_obj.insert(
                "audio_bitrate".to_owned(),
                serde_json::json!(raw_meta.audio_bitrate),
            );
            context_obj.insert(
                "audio_sample_rate".to_owned(),
                serde_json::json!(raw_meta.audio_sample_rate),
            );
            context_obj.insert(
                "audio_bit_depth".to_owned(),
                serde_json::json!(raw_meta.audio_bit_depth),
            );
            if !context_obj.contains_key("uploaded_by_user_id") {
                context_obj.insert(
                    "uploaded_by_user_id".to_owned(),
                    serde_json::json!(uploader.user_id),
                );
            }
            if !context_obj.contains_key("uploader_name") {
                context_obj.insert("uploader_name".to_owned(), serde_json::json!(uploader.name));
            }
        }

        prepared.push(PreparedFile {
            review,
            filename,
            raw_meta,
            hints,
            context,
        });
    }

    log.info(&format!(
        "Phase 1 done: {} prepared, {} merged duplicates, {} failed",
        prepared.len(),
        merged_count,
        failed_reviews.len(),
    ));

    if prepared.is_empty() {
        let duration_ms = batch_start.elapsed().as_millis() as i64;
        let _ = run.set_completed(db, duration_ms, &log.output()).await;
        return (merged_count, failed_reviews.len() as u64);
    }

    // Phase 2: RAG lookup (collect unique artist/album queries from all files)
    log.info("Phase 2: RAG lookup...");
    let mut artist_queries: Vec<String> = Vec::new();
    let mut album_queries: Vec<String> = Vec::new();

    for p in &prepared {
        let artist_q = p
            .raw_meta
            .artist
            .as_deref()
            .or(p.hints.artist.as_deref())
            .unwrap_or("")
            .to_owned();
        if !artist_q.is_empty() && !artist_queries.contains(&artist_q) {
            artist_queries.push(artist_q);
        }
        let album_q = p
            .raw_meta
            .album
            .as_deref()
            .or(p.hints.album.as_deref())
            .unwrap_or("")
            .to_owned();
        if !album_q.is_empty() && !album_queries.contains(&album_q) {
            album_queries.push(album_q);
        }
    }

    // Lookup all unique artist queries
    let mut all_similar_artists = Vec::new();
    for q in &artist_queries {
        let rag_start = std::time::Instant::now();
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            crate::agent::rag::find_similar_artists(pool, q, 5),
        )
        .await
        {
            Ok(Ok(results)) => {
                crate::metrics::record_agent_rag(
                    "artist",
                    "ok",
                    rag_start.elapsed(),
                    results.len(),
                );
                for a in results {
                    if !all_similar_artists
                        .iter()
                        .any(|x: &crate::agent::dto::SimilarArtist| x.id == a.id)
                    {
                        all_similar_artists.push(a);
                    }
                }
            }
            Ok(Err(e)) => {
                crate::metrics::record_agent_rag("artist", "error", rag_start.elapsed(), 0);
                log.warn(&format!("RAG artist lookup failed for \"{q}\": {e}"))
            }
            Err(_) => {
                crate::metrics::record_agent_rag("artist", "timeout", rag_start.elapsed(), 0);
                log.warn(&format!("RAG artist lookup timed out for \"{q}\""))
            }
        }
    }

    let mut all_similar_releases = Vec::new();
    for q in &album_queries {
        let rag_start = std::time::Instant::now();
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            crate::agent::rag::find_similar_releases(pool, q, 5),
        )
        .await
        {
            Ok(Ok(results)) => {
                crate::metrics::record_agent_rag(
                    "release",
                    "ok",
                    rag_start.elapsed(),
                    results.len(),
                );
                for r in results {
                    if !all_similar_releases
                        .iter()
                        .any(|x: &crate::agent::dto::SimilarRelease| x.id == r.id)
                    {
                        all_similar_releases.push(r);
                    }
                }
            }
            Ok(Err(e)) => {
                crate::metrics::record_agent_rag("release", "error", rag_start.elapsed(), 0);
                log.warn(&format!("RAG release lookup failed for \"{q}\": {e}"))
            }
            Err(_) => {
                crate::metrics::record_agent_rag("release", "timeout", rag_start.elapsed(), 0);
                log.warn(&format!("RAG release lookup timed out for \"{q}\""))
            }
        }
    }

    log.info(&format!(
        "Phase 2 done: {} similar artists, {} similar releases",
        all_similar_artists.len(),
        all_similar_releases.len(),
    ));

    // Phase 3: Build folder context and call batch LLM
    log.info("Phase 3: calling LLM (batch)...");

    // Build folder context from the first file's folder
    let folder_ctx = {
        let first_path = crate::media_paths::resolve_path_from_root(
            &config.agent_inbox_dir,
            prepared[0].review.input_path_str(),
        );
        let folder = first_path.parent().unwrap_or(first_path.as_path());
        let mut folder_files: Vec<String> = std::fs::read_dir(folder)
            .ok()
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
                        if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                            Some(name)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        folder_files.sort();
        let track_count = folder_files.len();
        FolderContext {
            folder_path: folder_rel.to_owned(),
            folder_files,
            track_count,
        }
    };

    // Build batch input
    let batch_files: Vec<BatchFileInput> = prepared
        .iter()
        .map(|p| BatchFileInput {
            filename: p.filename.clone(),
            raw: RawMetadata {
                title: p.raw_meta.title.clone(),
                artist: p.raw_meta.artist.clone(),
                album: p.raw_meta.album.clone(),
                track_number: p.raw_meta.track_number,
                year: p.raw_meta.year,
                genre: p.raw_meta.genre.clone(),
                duration_secs: p.raw_meta.duration_secs,
                audio_bitrate: p.raw_meta.audio_bitrate,
                audio_sample_rate: p.raw_meta.audio_sample_rate,
                audio_bit_depth: p.raw_meta.audio_bit_depth,
            },
            hints: PathHints {
                title: p.hints.title.clone(),
                artist: p.hints.artist.clone(),
                album: p.hints.album.clone(),
                year: p.hints.year,
                track_number: p.hints.track_number,
            },
        })
        .collect();

    let system_prompt = include_str!("../../prompts/normalize_batch.txt");
    let context_limit = config.agent_context_limit;

    let llm_result = crate::agent::normalize::normalize_batch(
        &config.agent_llm_url,
        &config.agent_llm_model,
        &config.agent_llm_auth,
        system_prompt,
        context_limit,
        batch_files,
        &all_similar_artists,
        &all_similar_releases,
        Some(&folder_ctx),
    )
    .await;

    let batch_result = match llm_result {
        Ok(r) => r,
        Err(e) => {
            crate::metrics::record_agent_failed("llm");
            crate::metrics::record_agent_folder_batch("failed", file_count, batch_start.elapsed());
            let err_msg = format!("Batch LLM call failed: {e}");
            log.error(&err_msg);
            // Mark all files as failed
            let prepared_count = prepared.len() as u64;
            for mut p in prepared {
                let _ = p.review.set_failed(db, &err_msg).await;
                crate::metrics::record_agent_file_processed("failed", "failed");
            }
            let total_fail_count = failed_reviews.len() as u64 + prepared_count;
            let duration_ms = batch_start.elapsed().as_millis() as i64;
            let _ = run
                .set_failed(db, duration_ms, &log.output(), &err_msg)
                .await;
            return (merged_count, total_fail_count);
        }
    };

    log.info(&format!(
        "Phase 3 done: LLM returned {} results in {}ms (model={}, tokens={}/{})",
        batch_result.results.len(),
        batch_result.duration_ms,
        batch_result.model,
        batch_result.prompt_tokens,
        batch_result.completion_tokens,
    ));

    // Phase 4: Match results to files and finalize
    log.info("Phase 4: finalizing...");

    // Build lookup map: filename → NormalizedFields
    let result_map: HashMap<String, NormalizedFields> = batch_result.results.into_iter().collect();

    let llm_model = &batch_result.model;
    let prompt_per_file = batch_result.prompt_tokens / prepared.len().max(1) as u64;
    let completion_per_file = batch_result.completion_tokens / prepared.len().max(1) as u64;
    let duration_per_file = batch_result.duration_ms as i64 / prepared.len().max(1) as i64;

    let mut ok_count = merged_count;
    let mut fail_count = failed_reviews.len() as u64;

    for mut p in prepared {
        let filename = &p.filename;

        let normalized = match result_map.get(filename) {
            Some(n) => n,
            None => {
                let msg = format!("LLM returned no result for \"{filename}\"");
                log.error(&msg);
                let _ = p.review.set_failed(db, &msg).await;
                fail_count += 1;
                continue;
            }
        };

        // Record processing stats
        let _ = ProcessingStats::create(
            db,
            p.review.id_val(),
            llm_model,
            duration_per_file,
            prompt_per_file as i64,
            completion_per_file as i64,
        )
        .await;

        let result_json = serde_json::to_string(normalized).unwrap_or_default();
        let confidence = normalized.confidence.unwrap_or(0.0);
        crate::metrics::observe_agent_confidence(confidence);

        let feat = if normalized.featured_artists.is_empty() {
            String::new()
        } else {
            format!(" feat=[{}]", normalized.featured_artists.join(", "))
        };
        log.info(&format!(
            "{filename}: artist={} | album={} | title={} | track={} | year={} | conf={}{}",
            normalized.artist.as_deref().unwrap_or("-"),
            normalized.album.as_deref().unwrap_or("-"),
            normalized.title.as_deref().unwrap_or("-"),
            normalized
                .track_number
                .map_or("-".into(), |n| n.to_string()),
            normalized.year.map_or("-".into(), |y| y.to_string()),
            confidence,
            feat,
        ));

        p.review.result_json = Some(result_json);
        let _ = p.review.save(db).await;

        let input_path = crate::media_paths::resolve_path_from_root(
            &config.agent_inbox_dir,
            p.review.input_path_str(),
        );
        let input_path_str = input_path.to_string_lossy().to_string();

        if confidence >= config.agent_confidence_threshold {
            match finalize_approved(
                db,
                pool,
                config,
                &input_path_str,
                normalized,
                &p.context,
                &config.agent_storage_dir,
                Some(llm_model),
            )
            .await
            {
                Ok(()) => {
                    let _ = p.review.set_auto_approved(db).await;
                    crate::metrics::record_agent_file_processed("ok", "auto_approved");
                    ok_count += 1;
                }
                Err(e) => {
                    crate::metrics::record_agent_failed("finalize");
                    crate::metrics::record_agent_file_processed("failed", "failed");
                    let msg = format!("{filename}: finalize failed: {e}");
                    log.error(&msg);
                    let _ = p.review.set_failed(db, &msg).await;
                    fail_count += 1;
                }
            }
        } else {
            p.review.status = cot::db::LimitedString::new("pending").unwrap();
            p.review.updated_at = cot::db::LimitedString::new(
                &chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            )
            .unwrap();
            let _ = p.review.save(db).await;
            crate::metrics::record_agent_file_processed("ok", "pending_review");
            log.info(&format!(
                "{filename}: manual review (confidence {confidence} < {})",
                config.agent_confidence_threshold,
            ));
            ok_count += 1; // Not a failure, just needs review
        }
    }

    let duration_ms = batch_start.elapsed().as_millis() as i64;
    if fail_count == 0 {
        let _ = run.set_completed(db, duration_ms, &log.output()).await;
        crate::metrics::record_agent_folder_batch("completed", file_count, batch_start.elapsed());
    } else {
        let msg = format!("{fail_count} file(s) failed");
        let _ = run.set_failed(db, duration_ms, &log.output(), &msg).await;
        crate::metrics::record_agent_folder_batch("failed", file_count, batch_start.elapsed());
    }

    (ok_count, fail_count)
}

// ---------------------------------------------------------------------------
// Finalization (called on approve or auto-approve)
// ---------------------------------------------------------------------------

pub async fn finalize_approved(
    db: &cot::db::Database,
    pool: &sqlx::PgPool,
    _config: &crate::config::AppConfig,
    input_path_str: &str,
    normalized: &NormalizedFields,
    context: &serde_json::Value,
    storage_dir_str: &str,
    model_name: Option<&str>,
) -> anyhow::Result<()> {
    let artist_name = normalized.artist.as_deref().unwrap_or("Unknown Artist");
    let release_title = normalized.album.as_deref().unwrap_or("Unknown Release");
    let track_title = normalized.title.as_deref().unwrap_or("Unknown Title");
    let release_type = normalized.release_type.as_deref().unwrap_or("album");
    let year = normalized.year;
    let track_number = normalized.track_number;

    let artist = find_or_create_artist(db, artist_name, model_name).await?;
    let release = find_or_create_release(db, release_title, release_type, year, model_name).await?;

    // Link ReleaseArtist
    let existing_links = ReleaseArtist::find_by_release(db, release.id_val())
        .await
        .unwrap_or_default();
    let already_linked = existing_links
        .iter()
        .any(|l| l.artist_id() == artist.id_val());
    if !already_linked {
        let position = existing_links.len() as i32;
        let mut link = ReleaseArtist {
            id: cot::db::Auto::auto(),
            release_id: release.id_val(),
            artist_id: artist.id_val(),
            position,
        };
        link.insert(db)
            .await
            .map_err(|e| anyhow::anyhow!("failed to link release-artist: {e}"))?;
    }

    let sha256 = context.get("sha256").and_then(|v| v.as_str()).unwrap_or("");
    let file_size = context
        .get("file_size")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let duration_secs = context
        .get("duration_secs")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let audio_bitrate = context
        .get("audio_bitrate")
        .and_then(|v| v.as_i64())
        .and_then(|v| i32::try_from(v).ok());
    let audio_sample_rate = context
        .get("audio_sample_rate")
        .and_then(|v| v.as_i64())
        .and_then(|v| i32::try_from(v).ok());
    let audio_bit_depth = context
        .get("audio_bit_depth")
        .and_then(|v| v.as_i64())
        .and_then(|v| i32::try_from(v).ok());
    let uploaded_by_user_id = context.get("uploaded_by_user_id").and_then(|v| v.as_i64());
    let uploader_name = context
        .get("uploader_name")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("UFO");

    let source_path = Path::new(input_path_str);
    let original_filename = source_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let ext = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("flac");

    let mime_type = match ext.to_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "aac" | "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "aiff" | "aif" => "audio/aiff",
        _ => "application/octet-stream",
    };

    let track_num = track_number.unwrap_or(0);
    let dest_filename = if track_num > 0 {
        format!(
            "{:02} - {}.{}",
            track_num,
            sanitize_filename(track_title),
            ext
        )
    } else {
        format!("{}.{}", sanitize_filename(track_title), ext)
    };

    let storage_dir = crate::media_paths::resolve_config_path_buf(storage_dir_str);
    let storage_path = if source_path.exists() {
        match mover::move_to_storage(
            &storage_dir,
            artist_name,
            release_title,
            &dest_filename,
            source_path,
        )
        .await?
        {
            mover::MoveOutcome::Moved(p) | mover::MoveOutcome::Merged(p) => {
                crate::media_paths::media_file_path_for_storage(storage_dir_str, &p).ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "storage destination is outside agent_storage_dir: {}",
                            p.display()
                        )
                    },
                )?
            }
        }
    } else {
        let expected_path = storage_dir
            .join(sanitize_filename(artist_name))
            .join(sanitize_filename(release_title))
            .join(&dest_filename);
        crate::media_paths::media_file_path_for_storage(storage_dir_str, &expected_path)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "storage destination is outside agent_storage_dir: {}",
                    expected_path.display()
                )
            })?
    };

    let reusable_media_id: Option<i64> = sqlx::query_scalar(
        r#"SELECT media.id
             FROM furumusic__media_file media
            WHERE media.file_type = 'audio'
              AND media.file_path = $1
              AND media.sha256_hash = $2
              AND NOT EXISTS (
                    SELECT 1 FROM furumusic__track track
                     WHERE track.audio_file_id = media.id OR track.cover_file_id = media.id
              )
              AND NOT EXISTS (
                    SELECT 1 FROM furumusic__release release
                     WHERE release.cover_file_id = media.id
              )
              AND NOT EXISTS (
                    SELECT 1 FROM furumusic__artist artist
                     WHERE artist.image_file_id = media.id
              )
              AND NOT EXISTS (
                    SELECT 1 FROM furumusic__playlist playlist
                     WHERE playlist.cover_file_id = media.id
              )
            ORDER BY media.id
            LIMIT 1"#,
    )
    .bind(&storage_path)
    .bind(sha256)
    .fetch_optional(pool)
    .await?;
    let media_file = if let Some(media_file_id) = reusable_media_id {
        MediaFile::get_by_id(db, media_file_id)
            .await
            .map_err(|error| anyhow::anyhow!("failed to load reusable media file: {error}"))?
            .ok_or_else(|| anyhow::anyhow!("reusable media file disappeared"))?
    } else {
        MediaFile::create(
            db,
            "audio",
            &storage_path,
            original_filename,
            mime_type,
            file_size,
            sha256,
            Some(ext),
            audio_bitrate,
            audio_sample_rate,
            audio_bit_depth,
            uploaded_by_user_id,
            Some(uploader_name),
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to create media file: {e}"))?
    };

    let track = Track::create(
        db,
        track_title,
        release.id_val(),
        track_number,
        None,
        duration_secs,
        media_file.id_val(),
        year,
        model_name,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to create track: {e}"))?;

    if let Err(error) = sqlx::query(
        r#"INSERT INTO furumusic__youtube_import_media (item_id, media_file_id)
           SELECT DISTINCT item.id, $1
             FROM furumusic__youtube_download_item item
             JOIN furumusic__pending_review review
               ON item.inbox_path IS NOT NULL
              AND (review.input_path = item.inbox_path
                   OR left(review.input_path, length(item.inbox_path) + 1)
                      = item.inbox_path || '/')
            WHERE review.context_json IS NOT NULL
              AND substring(
                      review.context_json
                      from '"sha256"[[:space:]]*:[[:space:]]*"([0-9a-fA-F]{64})"'
                  ) = $2
           ON CONFLICT (item_id, media_file_id) DO NOTHING"#,
    )
    .bind(media_file.id_val())
    .bind(sha256)
    .execute(pool)
    .await
    {
        tracing::warn!(
            track_id = track.id_val(),
            media_file_id = media_file.id_val(),
            error = %error,
            "failed to link imported media to its YouTube download item"
        );
    }

    TrackArtist::create(db, track.id_val(), artist.id_val(), "main", 0)
        .await
        .map_err(|e| anyhow::anyhow!("failed to link track-artist: {e}"))?;

    for (i, feat_name) in normalized.featured_artists.iter().enumerate() {
        let feat_artist = find_or_create_artist(db, feat_name, model_name).await?;
        let _ = TrackArtist::create(
            db,
            track.id_val(),
            feat_artist.id_val(),
            "featuring",
            (i + 1) as i32,
        )
        .await;
    }

    let approved_genre = normalized
        .genre
        .as_deref()
        .or_else(|| context.get("raw_genre").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(genre) = approved_genre {
        if let Err(err) =
            crate::jobs::metadata_backfill::save_approved_track_genres(pool, track.id_val(), genre)
                .await
        {
            tracing::warn!(
                track_id = track.id_val(),
                genre,
                error = %err,
                "failed to save approved track genre metadata"
            );
        }
    }

    // Cover art: if the release has no cover yet, try to find one
    if release.cover_file_id.is_none() {
        let source_folder = Path::new(input_path_str).parent().unwrap_or(Path::new("."));

        // Collect audio files in the same folder to try embedded extraction
        let audio_files_in_folder: Vec<std::path::PathBuf> = std::fs::read_dir(source_folder)
            .ok()
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
                        AUDIO_EXTENSIONS.contains(&ext.as_str())
                    })
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default();

        match crate::agent::cover_art::find_best_cover(source_folder, &audio_files_in_folder).await
        {
            Some(cover) => {
                let source_desc = match &cover.source {
                    crate::agent::cover_art::CoverSource::FolderFile(p) => {
                        format!("folder file: {}", p.display())
                    }
                    crate::agent::cover_art::CoverSource::Embedded(p) => {
                        format!("embedded in: {}", p.display())
                    }
                    crate::agent::cover_art::CoverSource::Remote(url) => {
                        format!("remote: {url}")
                    }
                };
                match crate::agent::cover_art::save_cover_to_storage(
                    db,
                    pool,
                    storage_dir_str,
                    artist_name,
                    release_title,
                    &cover,
                )
                .await
                {
                    Ok(cover_file_id) => {
                        let _ = crate::agent::cover_art::assign_cover_to_release(
                            pool,
                            release.id_val(),
                            cover_file_id,
                        )
                        .await;
                        tracing::info!(
                            release_id = release.id_val(),
                            cover_file_id,
                            source = %source_desc,
                            "Assigned cover art to release"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            release_id = release.id_val(),
                            error = %e,
                            "Failed to save cover art"
                        );
                    }
                }
            }
            None => {
                tracing::debug!(
                    release_id = release.id_val(),
                    "No cover art found for release"
                );
            }
        }
    }

    tracing::info!(
        track_id = track.id_val(),
        artist = artist_name,
        release = release_title,
        title = track_title,
        "Track finalized"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn find_or_create_artist(
    db: &cot::db::Database,
    name: &str,
    model_name: Option<&str>,
) -> anyhow::Result<Artist> {
    let name_sort = name.trim().to_lowercase();
    let all = Artist::list_all(db).await.unwrap_or_default();
    for a in &all {
        if a.name_sort.as_str() == name_sort {
            return Ok(a.clone());
        }
    }
    Artist::create(db, name, model_name)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create artist: {e}"))
}

async fn find_or_create_release(
    db: &cot::db::Database,
    title: &str,
    release_type: &str,
    year: Option<i32>,
    model_name: Option<&str>,
) -> anyhow::Result<Release> {
    let title_sort = title.trim().to_lowercase();
    let all = Release::list_all(db).await.unwrap_or_default();
    for r in &all {
        if r.title_sort.as_str() == title_sort && r.release_type.as_str() == release_type {
            return Ok(r.clone());
        }
    }
    Release::create(db, title, release_type, year, model_name)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create release: {e}"))
}

async fn cleanup_empty_dirs(dir: &Path) -> bool {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return false,
    };

    let mut is_empty = true;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let ft = match entry.file_type().await {
            Ok(ft) => ft,
            Err(_) => {
                is_empty = false;
                continue;
            }
        };
        if ft.is_dir() {
            let child_empty = Box::pin(cleanup_empty_dirs(&entry.path())).await;
            if child_empty {
                let _ = tokio::fs::remove_dir(&entry.path()).await;
            } else {
                is_empty = false;
            }
        } else {
            is_empty = false;
        }
    }
    is_empty
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn truncate_path(path: &str, max_len: usize) -> String {
    let char_count = path.chars().count();
    if char_count <= max_len {
        path.to_owned()
    } else if max_len <= 3 {
        ".".repeat(max_len)
    } else {
        let suffix: String = path.chars().skip(char_count - (max_len - 3)).collect();
        format!("...{suffix}")
    }
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }

    if max_bytes <= 3 {
        return ".".repeat(max_bytes);
    }

    let suffix_budget = max_bytes - 3;
    let mut suffix = Vec::new();
    let mut suffix_len = 0;
    for ch in value.chars().rev() {
        let ch_len = ch.len_utf8();
        if suffix_len + ch_len > suffix_budget {
            break;
        }
        suffix.push(ch);
        suffix_len += ch_len;
    }

    let mut result = String::from("...");
    for ch in suffix.iter().rev() {
        result.push(*ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{truncate_path, truncate_utf8_bytes};

    #[test]
    fn truncate_path_handles_unicode_boundaries() {
        assert_eq!(
            truncate_path("KUNTEYNIR/Блёвбургер", 20),
            "KUNTEYNIR/Блёвбургер"
        );
        assert_eq!(
            truncate_path("KUNTEYNIR/ОченьДлинноеНазвание", 12),
            "...еНазвание"
        );
    }

    #[test]
    fn truncate_utf8_bytes_handles_limited_string_boundaries() {
        let value = truncate_utf8_bytes("KUNTEYNIR/Блёвбургер(1)", 32);
        assert!(value.len() <= 32);
        assert!(value.is_char_boundary(value.len()));
        assert!(value.ends_with("Блёвбургер(1)"));
    }
}

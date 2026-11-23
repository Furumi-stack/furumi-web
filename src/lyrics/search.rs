//! Local, content-addressed lyrics retrieval. Text and audio profiles never mix.
//! Source text remains authoritative; the derived vector cache can be rebuilt.
use anyhow::{Context, Result};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use serde::Serialize;
use sqlx::{PgPool, Row};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use unicode_normalization::UnicodeNormalization;

#[cfg(test)]
#[path = "search_tests.rs"]
mod integration_tests;

// Includes the exact model revision, pooling, normalization and chunking recipe.
pub const PROFILE: &str = "lyrics-e5-small-614241f6-mean-l2-c240o60-v1";
pub const DIMENSIONS: usize = 384;
const BATCH: usize = 4;
const MAX_CHARS: usize = 240; // conservative even for scripts with many subword tokens
const OVERLAP: usize = 60;

#[derive(Clone, Serialize)]
pub struct Status {
    pub model: &'static str,
    pub profile: &'static str,
    pub dimensions: usize,
    pub enabled: bool,
    pub phase: String,
    pub texts: usize,
    pub ready_texts: usize,
    pub chunks: usize,
    pub ready_chunks: usize,
    pub stored_bytes: usize,
    pub last_error: Option<String>,
    pub workers: usize,
}
impl Default for Status {
    fn default() -> Self {
        Self {
            model: "multilingual-e5-small",
            profile: PROFILE,
            dimensions: DIMENSIONS,
            enabled: true,
            phase: "starting".into(),
            texts: 0,
            ready_texts: 0,
            chunks: 0,
            ready_chunks: 0,
            stored_bytes: 0,
            last_error: None,
            workers: 1,
        }
    }
}
#[derive(Clone)]
struct Chunk {
    hash: String,
    tokens: Vec<String>,
}
#[derive(Clone)]
struct Document {
    hash: String,
    id: String,
    stamp: String,
    chunks: Vec<Chunk>,
}
#[derive(Default, Clone)]
struct Corpus {
    docs: HashMap<String, Arc<Document>>,
    vectors: Arc<RwLock<HashMap<String, Arc<Vec<f32>>>>>,
    checked: HashSet<String>,
    refreshed: Option<Instant>,
}
pub struct Manager {
    enabled: AtomicBool,
    corpus: RwLock<Arc<Corpus>>,
    refresh_lock: AsyncMutex<()>,
    model: Mutex<Option<TextEmbedding>>,
    model_retry: Mutex<Option<Instant>>,
    status: RwLock<Status>,
    query_slots: Semaphore,
    pending: Mutex<VecDeque<Chunk>>,
}
pub fn handle() -> &'static Arc<Manager> {
    static HANDLE: OnceLock<Arc<Manager>> = OnceLock::new();
    HANDLE.get_or_init(|| {
        Arc::new(Manager {
            enabled: AtomicBool::new(true),
            corpus: RwLock::new(Arc::new(Corpus::default())),
            refresh_lock: AsyncMutex::new(()),
            model: Mutex::new(None),
            model_retry: Mutex::new(None),
            status: RwLock::new(Status::default()),
            query_slots: Semaphore::new(2),
            pending: Mutex::new(VecDeque::new()),
        })
    })
}
impl Manager {
    pub fn status(&self) -> Status {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn apply(&self, config: &crate::config::AppConfig) {
        self.enabled
            .store(config.lyrics_indexing_enabled, Ordering::Release);
        let mut status = self.status.write().unwrap_or_else(|e| e.into_inner());
        status.enabled = config.lyrics_indexing_enabled;
        if !status.enabled {
            status.phase = "paused".into();
        } else if status.phase == "paused" {
            status.phase = "indexing".into();
        }
    }
    pub async fn boot(self: &Arc<Self>, config: &crate::config::AppConfig) {
        self.apply(config);
        if config.database_url.trim().is_empty() {
            return;
        }
        let pool = loop {
            match sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect(&config.database_url)
                .await
            {
                Ok(pool) => break pool,
                Err(error) => {
                    self.error(format!("database: {error}"));
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        };
        loop {
            // Database overrides also apply on restart. Environment always wins.
            if let Ok(Some(value)) = sqlx::query_scalar::<_, String>(
                "SELECT value FROM furumusic__config_entry WHERE key='lyrics_indexing_enabled'",
            )
            .fetch_optional(&pool)
            .await
            {
                let mut effective = config.clone();
                if let Ok(value) = value.parse() {
                    effective.lyrics_indexing_enabled = value;
                }
                effective.apply_env_overrides();
                self.apply(&effective);
            }
            match self.refresh(&pool, false).await {
                Ok(()) => {
                    if self.enabled.load(Ordering::Acquire) {
                        if let Err(error) = self.index_batch(&pool).await {
                            self.error(format!("{error:#}"));
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        } else if !self
                            .pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_empty()
                        {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    }
                }
                Err(error) => self.error(format!("{error:#}")),
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
    fn error(&self, error: String) {
        tracing::warn!(%error,"lyrics search");
        let mut status = self.status.write().unwrap_or_else(|e| e.into_inner());
        status.phase = "error".into();
        status.last_error = Some(error);
    }
    async fn refresh(&self, pool: &PgPool, force: bool) -> Result<()> {
        let _guard = self.refresh_lock.lock().await;
        let old = self
            .corpus
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !force
            && old
                .refreshed
                .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
        {
            return Ok(());
        }
        // Read inexpensive identities first; large texts are only transferred when changed.
        let rows=sqlx::query("SELECT l.audio_sha256,l.id,l.updated_at::text AS stamp FROM furumusic__lyrics l WHERE NOT l.instrumental AND EXISTS(SELECT 1 FROM furumusic__media_file m JOIN furumusic__track t ON t.audio_file_id=m.id JOIN furumusic__release r ON r.id=t.release_id WHERE m.sha256_hash=l.audio_sha256 AND NOT t.is_hidden AND NOT r.is_hidden)").fetch_all(pool).await?;
        let mut next = Corpus {
            vectors: old.vectors.clone(),
            checked: old.checked.clone(),
            ..Default::default()
        };
        let mut changed = HashSet::new();
        for row in rows {
            let hash: String = row.get("audio_sha256");
            let id: String = row.get("id");
            let stamp: String = row.get("stamp");
            let key = format!("{hash}/{id}");
            if let Some(doc) = old.docs.get(&key).filter(|doc| doc.stamp == stamp) {
                next.docs.insert(key, doc.clone());
            } else {
                changed.insert(hash);
            }
        }
        if !changed.is_empty() {
            let hashes: Vec<_> = changed.into_iter().collect();
            for row in sqlx::query("SELECT audio_sha256,id,plain_text,synced_lrc,updated_at::text AS stamp FROM furumusic__lyrics WHERE audio_sha256=ANY($1) AND NOT instrumental").bind(&hashes).fetch_all(pool).await? {
                let plain:Option<String>=row.get("plain_text");let lrc:Option<String>=row.get("synced_lrc");
                let text=plain.filter(|s|!s.trim().is_empty()).unwrap_or_else(||super::parse_lrc(lrc.as_deref().unwrap_or("")).into_iter().map(|(_,s)|s).collect::<Vec<_>>().join("\n"));
                let hash:String=row.get("audio_sha256");let id:String=row.get("id");
                let doc=Document{hash:hash.clone(),id:id.clone(),stamp:row.get("stamp"),chunks:chunks(&text)};
                if !doc.chunks.is_empty(){next.docs.insert(format!("{hash}/{id}"),Arc::new(doc));}
            }
        }
        let needed: HashSet<_> = next
            .docs
            .values()
            .flat_map(|d| d.chunks.iter().map(|c| c.hash.clone()))
            .collect();
        next.vectors
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| needed.contains(key));
        next.checked.retain(|key| needed.contains(key));
        let missing: Vec<_> = needed
            .iter()
            .filter(|k| !next.checked.contains(*k))
            .cloned()
            .collect();
        for batch in missing.chunks(2000) {
            for row in sqlx::query("SELECT text_hash,vector FROM furumusic__lyrics_embedding WHERE profile=$1 AND text_hash=ANY($2)").bind(PROFILE).bind(batch).fetch_all(pool).await? {
                let bytes:Vec<u8>=row.get("vector");
                if let Some(v)=decode_vector(&bytes){next.vectors.write().unwrap_or_else(|e|e.into_inner()).insert(row.get("text_hash"),Arc::new(v));}
            }
        }
        next.checked.extend(missing);
        {
            let vectors = next.vectors.read().unwrap_or_else(|e| e.into_inner());
            let mut seen = HashSet::new();
            *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = next
                .docs
                .values()
                .flat_map(|d| &d.chunks)
                .filter(|c| !vectors.contains_key(&c.hash) && seen.insert(c.hash.clone()))
                .cloned()
                .collect();
        }
        next.refreshed = Some(Instant::now());
        self.update_counts(&next);
        *self.corpus.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
        Ok(())
    }
    fn update_counts(&self, corpus: &Corpus) {
        let vectors = corpus.vectors.read().unwrap_or_else(|e| e.into_inner());
        let mut status = self.status.write().unwrap_or_else(|e| e.into_inner());
        let unique: HashSet<_> = corpus
            .docs
            .values()
            .map(|d| d.chunks.iter().map(|c| c.hash.as_str()).collect::<Vec<_>>())
            .collect();
        status.texts = unique.len();
        status.ready_texts = unique
            .iter()
            .filter(|doc| doc.iter().all(|hash| vectors.contains_key(*hash)))
            .count();
        status.chunks = corpus
            .docs
            .values()
            .flat_map(|d| d.chunks.iter().map(|c| &c.hash))
            .collect::<HashSet<_>>()
            .len();
        status.ready_chunks = vectors.len();
        status.stored_bytes = status.ready_chunks * DIMENSIONS * 4;
        status.phase = if !self.enabled.load(Ordering::Acquire) {
            "paused"
        } else if status.ready_chunks == status.chunks {
            "ready"
        } else {
            "indexing"
        }
        .into();
    }
    async fn embed(self: &Arc<Self>, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut model = manager.model.lock().unwrap_or_else(|e| e.into_inner());
            if model.is_none() {
                let mut retry = manager
                    .model_retry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                anyhow::ensure!(
                    !retry.is_some_and(|t| t.elapsed() < Duration::from_secs(60)),
                    "model unavailable; retry in one minute"
                );
                *retry = Some(Instant::now());
                *model = Some(load_model()?);
                *retry = None;
            }
            let output = model
                .as_mut()
                .unwrap()
                .embed(texts, Some(BATCH))
                .map_err(|e| anyhow::anyhow!("inference: {e}"))?;
            for vector in &output {
                anyhow::ensure!(
                    vector.len() == DIMENSIONS && vector.iter().all(|v| v.is_finite()),
                    "invalid lyrics embedding"
                );
            }
            Ok(output)
        })
        .await
        .context("lyrics inference worker")?
    }
    async fn index_batch(self: &Arc<Self>, pool: &PgPool) -> Result<()> {
        // Also serialize overlapping replicas during rolling deploys. Re-read this
        // batch inside the transaction so another replica's work is reused.
        let mut tx = pool.begin().await?;
        let locked: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,0))")
                .bind(PROFILE)
                .fetch_one(&mut *tx)
                .await?;
        if !locked {
            return Ok(());
        }
        let pending: Vec<_> = {
            let mut queue = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            (0..BATCH).filter_map(|_| queue.pop_front()).collect()
        };
        if pending.is_empty() {
            return Ok(());
        }
        let hashes: Vec<_> = pending.iter().map(|c| c.hash.clone()).collect();
        let mut vectors: HashMap<String, Vec<f32>> = HashMap::new();
        for row in sqlx::query("SELECT text_hash,vector FROM furumusic__lyrics_embedding WHERE profile=$1 AND text_hash=ANY($2)").bind(PROFILE).bind(&hashes).fetch_all(&mut *tx).await? {
            let bytes:Vec<u8>=row.get("vector");if let Some(vector)=decode_vector(&bytes){vectors.insert(row.get("text_hash"),vector);}
        }
        let missing: Vec<_> = pending
            .iter()
            .filter(|c| !vectors.contains_key(&c.hash))
            .collect();
        if !missing.is_empty() {
            let calculated = self
                .embed(
                    missing
                        .iter()
                        .map(|c| format!("passage: {}", c.tokens.join(" ")))
                        .collect(),
                )
                .await?;
            for (chunk, vector) in missing.into_iter().zip(calculated) {
                let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
                sqlx::query("INSERT INTO furumusic__lyrics_embedding(profile,text_hash,vector) VALUES($1,$2,$3) ON CONFLICT(profile,text_hash) DO UPDATE SET vector=EXCLUDED.vector").bind(PROFILE).bind(&chunk.hash).bind(bytes).execute(&mut *tx).await?;
                vectors.insert(chunk.hash.clone(), vector);
            }
        }
        tx.commit().await?;
        let _guard = self.refresh_lock.lock().await;
        let current = self
            .corpus
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        {
            let mut cache = current.vectors.write().unwrap_or_else(|e| e.into_inner());
            for (hash, vector) in vectors {
                if current.checked.contains(&hash) {
                    cache.insert(hash, Arc::new(vector));
                }
            }
            let mut status = self.status.write().unwrap_or_else(|e| e.into_inner());
            status.ready_chunks = cache.len();
            status.stored_bytes = cache.len() * DIMENSIONS * 4;
            status.last_error = None;
        }
        if self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            self.update_counts(&current);
        }
        Ok(())
    }
    pub async fn search(
        self: &Arc<Self>,
        pool: &PgPool,
        query: &str,
        limit: usize,
    ) -> Result<SearchOutput> {
        if query.trim().is_empty() {
            return Ok(SearchOutput {
                hits: Vec::new(),
                semantic_error: None,
            });
        }
        let _slot = tokio::time::timeout(Duration::from_secs(10), self.query_slots.acquire())
            .await
            .context("lyrics search busy")??;
        self.refresh(pool, false).await?;
        let corpus = self
            .corpus
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut semantic_error = None;
        let vector = if corpus
            .vectors
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            None
        } else {
            match self.embed(vec![format!("query: {query}")]).await {
                Ok(mut v) => v.pop(),
                Err(error) => {
                    semantic_error = Some(format!("{error:#}"));
                    None
                }
            }
        };
        let q = query.to_owned();
        let search_corpus = corpus.clone();
        let ranked =
            tokio::task::spawn_blocking(move || rank(&search_corpus, &q, vector.as_deref()))
                .await?;
        // Resolve against current visibility AND version timestamp: removed/edited text never leaks from a stale snapshot.
        let hashes: Vec<_> = ranked.iter().map(|hit| hit.hash.clone()).collect();
        let rows=sqlx::query("SELECT t.id,l.audio_sha256,l.id AS version_id,l.updated_at::text AS stamp FROM furumusic__track t JOIN furumusic__release r ON r.id=t.release_id JOIN furumusic__media_file m ON m.id=t.audio_file_id JOIN furumusic__lyrics l ON l.audio_sha256=m.sha256_hash WHERE l.audio_sha256=ANY($1) AND NOT t.is_hidden AND NOT r.is_hidden ORDER BY t.id").bind(hashes).fetch_all(pool).await?;
        let mut track_map: HashMap<(String, String, String), Vec<i64>> = HashMap::new();
        for row in rows {
            track_map
                .entry((
                    row.get("audio_sha256"),
                    row.get("version_id"),
                    row.get("stamp"),
                ))
                .or_default()
                .push(row.get("id"));
        }
        let mut hits = Vec::new();
        let mut seen = HashSet::new();
        for hit in ranked {
            if let Some(ids) =
                track_map.get(&(hit.hash.clone(), hit.version_id.clone(), hit.stamp.clone()))
            {
                for id in ids {
                    if seen.insert(*id) {
                        let mut h = hit.clone();
                        h.track_id = *id;
                        hits.push(h);
                        if hits.len() >= limit {
                            return Ok(SearchOutput {
                                hits,
                                semantic_error,
                            });
                        }
                    }
                }
            }
        }
        Ok(SearchOutput {
            hits,
            semantic_error,
        })
    }
}

fn load_model() -> Result<TextEmbedding> {
    let dir = std::env::var_os("FURU_LYRICS_MODEL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/opt/furumusic/models/multilingual-e5-small".into());
    let read = |file: &str| {
        std::fs::read(dir.join(file)).with_context(|| {
            format!(
                "missing bundled model file {} (set FURU_LYRICS_MODEL_DIR for development)",
                dir.join(file).display()
            )
        })
    };
    let files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let model =
        UserDefinedEmbeddingModel::new(read("model.onnx")?, files).with_pooling(Pooling::Mean);
    TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new()
            .with_max_length(512)
            .with_intra_threads(2),
    )
    .map_err(|e| anyhow::anyhow!("loading multilingual-e5-small: {e}"))
}
fn normalize(text: &str) -> String {
    text.nfkc()
        .flat_map(char::to_lowercase)
        .map(|c| if c == 'ё' { 'е' } else { c })
        .collect()
}
fn tokens(text: &str) -> Vec<String> {
    normalize(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
fn chunks(text: &str) -> Vec<Chunk> {
    let text = text.replace('\r', "");
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut seen = HashSet::new();
    while start < chars.len() {
        let max = (start + MAX_CHARS).min(chars.len());
        let mut end = max;
        if max < chars.len() {
            if let Some(offset) = chars[start + MAX_CHARS / 2..max]
                .iter()
                .rposition(|c| c.is_whitespace())
            {
                end = start + MAX_CHARS / 2 + offset;
            }
        }
        let slice: String = chars[start..end].iter().collect();
        let ts = tokens(&slice);
        if !ts.is_empty() {
            let canonical = ts.join(" ");
            let hash = blake3::hash(canonical.as_bytes()).to_hex().to_string();
            if seen.insert(hash.clone()) {
                out.push(Chunk { hash, tokens: ts });
            }
        }
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(OVERLAP).max(start + 1);
        while start < end && !chars[start - 1].is_whitespace() {
            start += 1;
        }
    }
    out
}
fn decode_vector(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() != DIMENSIONS * 4 {
        return None;
    }
    let vector: Vec<_> = bytes
        .chunks_exact(4)
        .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
        .collect();
    (vector.iter().all(|v| v.is_finite()) && vector.iter().any(|v| *v != 0.)).then_some(vector)
}
#[derive(Clone)]
pub struct Hit {
    pub track_id: i64,
    hash: String,
    stamp: String,
    version_id: String,
    score: f64,
    #[cfg(test)]
    lexical_score: f64,
}
pub struct SearchOutput {
    pub hits: Vec<Hit>,
    pub semantic_error: Option<String>,
}
fn rank(corpus: &Corpus, query: &str, vector: Option<&[f32]>) -> Vec<Hit> {
    let query_tokens = tokens(query);
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let stop:HashSet<_>="и в во на не что там где про как а но это то the a an is of to in it and something about where".split_whitespace().collect();
    let terms: Vec<_> = query_tokens
        .iter()
        .filter(|s| !stop.contains(s.as_str()))
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let terms = if terms.is_empty() {
        query_tokens.clone()
    } else {
        terms
    };
    let unique_chunks: HashMap<_, _> = corpus
        .docs
        .values()
        .flat_map(|d| &d.chunks)
        .map(|c| (&c.hash, c))
        .collect();
    let mut weights = HashMap::new();
    for term in &terms {
        let df = unique_chunks
            .values()
            .filter(|c| c.tokens.contains(term))
            .count();
        weights.insert(
            term,
            1. + ((unique_chunks.len() + 1) as f64 / (df + 1) as f64).ln(),
        );
    }
    let total: f64 = weights.values().sum();
    let vectors = corpus.vectors.read().unwrap_or_else(|e| e.into_inner());
    let mut scores = HashMap::new();
    for chunk in unique_chunks.values() {
        let coverage = terms
            .iter()
            .filter(|t| chunk.tokens.contains(t))
            .map(|t| weights[t])
            .sum::<f64>()
            / total;
        let mut longest = 0;
        if coverage > 0. {
            let mut previous = vec![0usize; chunk.tokens.len() + 1];
            for term in &query_tokens {
                let mut current = vec![0usize; chunk.tokens.len() + 1];
                for (i, word) in chunk.tokens.iter().enumerate() {
                    if word == term {
                        current[i + 1] = (previous[i] + 1).min(12);
                        longest = longest.max(current[i + 1]);
                    }
                }
                previous = current;
            }
        }
        let phrase = longest as f64 / query_tokens.len().min(12) as f64;
        let positions: Vec<_> = chunk
            .tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| terms.contains(t))
            .map(|(i, _)| i)
            .collect();
        let proximity = if positions.len() > 1 {
            terms.len().min(positions.len()) as f64
                / (positions.last().unwrap() - positions[0] + 1) as f64
        } else {
            0.
        };
        let lexical = (0.65 * coverage + 0.3 * phrase + 0.05 * proximity).min(1.);
        let semantic = vector.and_then(|q| {
            vectors.get(&chunk.hash).map(|v| {
                q.iter()
                    .zip(v.iter())
                    .map(|(a, b)| (*a as f64) * (*b as f64))
                    .sum::<f64>()
                    .clamp(-1., 1.)
            })
        });
        // E5 cosine scores are concentrated high; calibrate the useful interval.
        // Exact evidence dominates, semantic agreement breaks ties and rescues paraphrases.
        let dense = semantic
            .map(|s| ((s - 0.65) / 0.3).clamp(0., 1.))
            .unwrap_or(0.);
        let score = if semantic.is_some() {
            0.7 * lexical + 0.3 * dense + 0.15 * lexical * dense
        } else {
            lexical
        };
        if lexical > 0.12 || semantic.is_some_and(|s| s >= 0.78) {
            scores.insert(chunk.hash.clone(), (score, lexical, semantic));
        }
    }
    let mut hits = Vec::new();
    for doc in corpus.docs.values() {
        let best = doc
            .chunks
            .iter()
            .filter_map(|c| scores.get(&c.hash).map(|score| (c, score)))
            .max_by(|a, b| a.1.0.total_cmp(&b.1.0));
        if let Some((_chunk, (score, _lexical, _semantic))) = best {
            hits.push(Hit {
                track_id: 0,
                hash: doc.hash.clone(),
                stamp: doc.stamp.clone(),
                version_id: doc.id.clone(),
                score: *score,
                #[cfg(test)]
                lexical_score: *_lexical,
            });
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.hash.cmp(&b.hash))
            .then(a.version_id.cmp(&b.version_id))
    });
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_are_bounded_overlapping_unicode_and_deduplicated() {
        let input = "Снег ложится на могилы, и весна уже не придёт.\n".repeat(40);
        let cs = chunks(&input);
        assert!(cs.len() > 1);
        assert!(
            cs.iter()
                .all(|c| c.tokens.join(" ").chars().count() <= MAX_CHARS)
        );
        assert_eq!(
            cs.len(),
            cs.iter().map(|c| &c.hash).collect::<HashSet<_>>().len()
        );
        assert!(chunks("[ ]\n").is_empty());
    }
    fn corpus(texts: &[&str]) -> Corpus {
        let mut c = Corpus::default();
        for (i, text) in texts.iter().enumerate() {
            c.docs.insert(
                i.to_string(),
                Arc::new(Document {
                    hash: i.to_string(),
                    id: "v1".into(),
                    stamp: "now".into(),
                    chunks: chunks(text),
                }),
            );
        }
        c
    }
    #[test]
    fn exact_evidence_outranks_common_words_and_repeated_chorus() {
        let c = corpus(&[
            "Не будет весны. Просыпется снег на могилы.\nИ тишина останется с нами.",
            &"там где там где там где будет и что про".repeat(100),
            "Наступила весна, растаял снег и выросли цветы.",
        ]);
        let hits = rank(
            &c,
            "там где не будет весны что-то про просыпется снег на могилы",
            None,
        );
        assert_eq!(hits[0].hash, "0");
        assert!(hits[0].lexical_score > 0.7);
    }
    #[test]
    fn lexical_available_without_vectors_and_yo_normalized() {
        let c = corpus(&[
            "Ещё идёт снег. Всё замёрзло.",
            "The sun shines over the sea",
        ]);
        assert_eq!(rank(&c, "еще идет снег", None)[0].hash, "0");
        assert!(rank(&c, "несуществующаяфраза", None).is_empty());
    }
    #[test]
    fn combined_match_beats_semantic_only() {
        let c = corpus(&["snow falls on graves", "summer and sunshine"]);
        for doc in c.docs.values() {
            for chunk in &doc.chunks {
                let mut v = vec![0.; DIMENSIONS];
                v[0] = if doc.hash == "0" { 0.88 } else { 0.96 };
                c.vectors
                    .write()
                    .unwrap()
                    .insert(chunk.hash.clone(), Arc::new(v));
            }
        }
        let mut q = vec![0.; DIMENSIONS];
        q[0] = 1.;
        assert_eq!(rank(&c, "snow falls on graves", Some(&q))[0].hash, "0");
    }
    #[test]
    fn malformed_vectors_are_rejected() {
        assert!(decode_vector(&[0; 4]).is_none());
        assert!(decode_vector(&vec![0; DIMENSIONS * 4]).is_none());
    }
    #[test]
    #[ignore = "requires bundled model; FURU_LYRICS_MODEL_DIR"]
    fn real_e5_multilingual_retrieval() {
        let mut model = load_model().unwrap();
        let v = model
            .embed(
                vec![
                    "query: снег на могилы и не будет весны",
                    "passage: Просыпется снег на могилы, и больше не будет весны.",
                    "passage: We dance at the beach in summer sunshine.",
                ],
                Some(3),
            )
            .unwrap();
        assert_eq!(v[0].len(), 384);
        let dot = |a: &Vec<f32>, b: &Vec<f32>| a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>();
        assert!(dot(&v[0], &v[1]) > dot(&v[0], &v[2]) + 0.04);
        assert!((dot(&v[0], &v[0]) - 1.).abs() < 0.001);
    }
}

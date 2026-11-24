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
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, OnceCell, Semaphore, watch};
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
    enabled: watch::Sender<bool>,
    corpus: RwLock<Arc<Corpus>>,
    refresh_lock: AsyncMutex<()>,
    model: Mutex<Option<TextEmbedding>>,
    model_dir: OnceCell<PathBuf>,
    model_retry: Mutex<Option<Instant>>,
    status: RwLock<Status>,
    query_slots: Semaphore,
    pending: Mutex<VecDeque<Chunk>>,
}
pub fn handle() -> &'static Arc<Manager> {
    static HANDLE: OnceLock<Arc<Manager>> = OnceLock::new();
    HANDLE.get_or_init(|| Arc::new(Manager::default()))
}
impl Default for Manager {
    fn default() -> Self {
        Self {
            enabled: watch::channel(true).0,
            corpus: RwLock::new(Arc::new(Corpus::default())),
            refresh_lock: AsyncMutex::new(()),
            model: Mutex::new(None),
            model_dir: OnceCell::new(),
            model_retry: Mutex::new(None),
            status: RwLock::new(Status::default()),
            query_slots: Semaphore::new(2),
            pending: Mutex::new(VecDeque::new()),
        }
    }
}
impl Manager {
    pub fn status(&self) -> Status {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn apply(&self, config: &crate::config::AppConfig) {
        self.set_enabled(config.lyrics_indexing_enabled, None);
    }
    fn set_enabled(&self, enabled: bool, snapshot: Option<&watch::Receiver<bool>>) -> bool {
        let mut status = self.status.write().unwrap_or_else(|e| e.into_inner());
        // A DB read started before an admin update must not undo that update.
        if snapshot.is_some_and(|s| s.has_changed().unwrap_or(false)) {
            return false;
        }
        self.enabled.send_if_modified(|old| {
            let changed = *old != enabled;
            *old = enabled;
            changed
        });
        status.enabled = enabled;
        if !status.enabled {
            status.phase = "paused".into();
            status.last_error = None;
        } else if status.phase == "paused" {
            status.phase = "indexing".into();
        }
        true
    }
    pub async fn boot(self: &Arc<Self>, config: &crate::config::AppConfig) {
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
        let mut settings = self.enabled.subscribe();
        loop {
            let delay = match self.worker_step(&pool, config, &mut settings).await {
                Ok(delay) => delay,
                Err(error) => {
                    self.error(format!("{error:#}"));
                    Duration::from_secs(30)
                }
            };
            tokio::select! {
                _ = settings.changed() => (),
                _ = tokio::time::sleep(delay) => (),
            }
        }
    }

    async fn worker_step(
        self: &Arc<Self>,
        pool: &PgPool,
        config: &crate::config::AppConfig,
        settings: &mut watch::Receiver<bool>,
    ) -> Result<Duration> {
        // Read the persisted switch before attempting the first download.
        settings.borrow_and_update();
        let value = sqlx::query_scalar::<_, String>(
            "SELECT value FROM furumusic__config_entry WHERE key='lyrics_indexing_enabled'",
        )
        .fetch_optional(pool)
        .await?;
        let mut effective = config.clone();
        if let Some(value) = value.and_then(|v| v.parse().ok()) {
            effective.lyrics_indexing_enabled = value;
        }
        effective.apply_env_overrides();
        if !self.set_enabled(effective.lyrics_indexing_enabled, Some(settings)) {
            return Ok(Duration::ZERO);
        }
        let enabled = *settings.borrow_and_update();
        if enabled && self.model_dir.get().is_none() {
            self.status.write().unwrap_or_else(|e| e.into_inner()).phase = "downloading".into();
            tokio::select! {
                biased;
                _ = settings.changed() => return Ok(Duration::ZERO),
                result = self.model_dir.get_or_try_init(|| super::model_files::prepare(true)) => { result?; }
            }
            *self.model_retry.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let mut status = self.status.write().unwrap_or_else(|e| e.into_inner());
            status.last_error = None;
            status.phase = if status.ready_chunks == status.chunks {
                "ready"
            } else {
                "indexing"
            }
            .into();
        }
        self.refresh(pool, false).await?;
        if *self.enabled.borrow() {
            self.index_batch(pool).await?;
            if !self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            {
                return Ok(Duration::from_millis(100));
            }
        }
        Ok(Duration::from_secs(30))
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
        if status.enabled && self.model_dir.get().is_none() {
            return;
        }
        status.phase = if !status.enabled {
            "paused"
        } else if status.ready_chunks == status.chunks {
            "ready"
        } else {
            "indexing"
        }
        .into();
    }
    fn start_model_attempt(&self) -> Result<()> {
        let mut retry = self.model_retry.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            !retry.is_some_and(|t| t.elapsed() < Duration::from_secs(60)),
            "model unavailable; retry in one minute"
        );
        *retry = Some(Instant::now());
        Ok(())
    }
    async fn embed(self: &Arc<Self>, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        // Search may use an earlier index while indexing is paused. Only the
        // background worker downloads files; queries never wait for the download.
        let dir = if let Some(dir) = self.model_dir.get() {
            dir
        } else if !*self.enabled.borrow() {
            self.model_dir
                .get_or_try_init(|| async {
                    self.start_model_attempt()?;
                    let dir = super::model_files::prepare(false).await?;
                    *self.model_retry.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    Ok::<_, anyhow::Error>(dir)
                })
                .await?
        } else {
            anyhow::bail!("lyrics model is not ready");
        }
        .clone();
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut model = manager.model.lock().unwrap_or_else(|e| e.into_inner());
            if model.is_none() {
                manager.start_model_attempt()?;
                *model = Some(load_model(&dir)?);
                *manager
                    .model_retry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
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

fn load_model(dir: &Path) -> Result<TextEmbedding> {
    let read = |file: &str| {
        std::fs::read(dir.join(file))
            .with_context(|| format!("reading {}", dir.join(file).display()))
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

struct QueryTerms<'a> {
    words: &'a [String],
    positions: HashMap<&'a str, Vec<usize>>,
    idf: HashMap<&'a str, f64>,
    total_idf: f64,
    // Indexed by query start and span length minus one.
    quotes: Vec<Vec<f64>>,
    informative_len: usize,
}

#[derive(Default)]
struct LexicalMatch {
    score: f64,
    coverage: f64,
    quote: f64,
}

impl<'a> QueryTerms<'a> {
    fn new(words: &'a [String], chunks: &[&Chunk], hybrid: bool) -> Self {
        const STOP: &str = "и в во на не что там где про как а но это то the a an is of to in it and something about where";
        // Ignore filler when mixing scores. Literal spans still include these
        // words, and lexical-only searches keep their original vocabulary.
        const FILLER: &str = "я ты он она они мы вы у к с со по из за от для мне его ее или же бы был была было that on with from was he she they i";
        let mut stop: HashSet<_> = STOP.split_whitespace().collect();
        if hybrid {
            stop.extend(FILLER.split_whitespace());
        }
        let mut positions = HashMap::<_, Vec<_>>::new();
        for (index, word) in words.iter().enumerate() {
            positions.entry(word.as_str()).or_default().push(index);
        }
        let mut terms: Vec<_> = positions
            .keys()
            .copied()
            .filter(|s| !stop.contains(s))
            .collect();
        let informative_len = terms.len();
        if terms.is_empty() {
            terms.extend(positions.keys().copied());
        }
        terms.sort_unstable();
        let mut frequencies: HashMap<_, _> = terms.iter().map(|&s| (s, (0, None))).collect();
        for (index, chunk) in chunks.iter().enumerate() {
            for word in &chunk.tokens {
                if let Some((count, last)) = frequencies.get_mut(word.as_str())
                    && *last != Some(index)
                {
                    *count += 1;
                    *last = Some(index);
                }
            }
        }
        let n = (chunks.len() + 1) as f64;
        let idf: HashMap<_, _> = frequencies
            .into_iter()
            .map(|(word, (df, _))| (word, 1. + (n / (df + 1) as f64).ln()))
            .collect();
        let total_idf = terms.iter().map(|word| idf[word]).sum();
        let quotes = if hybrid {
            Self::quote_strengths(words, &idf, 1. + n.ln())
        } else {
            Vec::new()
        };
        Self {
            words,
            positions,
            idf,
            total_idf,
            quotes,
            informative_len,
        }
    }

    fn quote_strengths(words: &[String], idf: &HashMap<&str, f64>, max_idf: f64) -> Vec<Vec<f64>> {
        // Precompute evidence for every span of the bounded query once. Joined
        // rare words are more specific than common words or repeated occurrences.
        let mut quotes = Vec::with_capacity(words.len());
        for start in 0..words.len() {
            let mut seen = HashSet::new();
            let mut mass = 0.;
            let mut row = Vec::with_capacity(words.len() - start);
            for word in &words[start..] {
                if let Some(weight) = idf.get(word.as_str())
                    && seen.insert(word.as_str())
                {
                    mass += weight;
                }
                row.push(if seen.len() >= 2 {
                    (mass / (3. * max_idf)).min(1.)
                } else {
                    0.
                });
            }
            quotes.push(row);
        }
        // Even a short or common verbatim query is a useful direct match.
        if let Some(full) = quotes.first_mut().and_then(|row| row.last_mut()) {
            *full = 1.;
        }
        quotes
    }

    fn lexical_match(&self, chunk: &Chunk) -> LexicalMatch {
        let mut matched = HashSet::new();
        let mut matched_idf = 0.;
        let mut first = None;
        let mut last = 0;
        let mut occurrences = 0;
        let mut longest = 0;
        let mut quote: f64 = 0.;

        for (i, word) in chunk.tokens.iter().map(String::as_str).enumerate() {
            let Some(starts) = self.positions.get(word) else {
                continue;
            };
            if let Some(weight) = self.idf.get(word) {
                if matched.insert(word) {
                    matched_idf += weight;
                }
                first.get_or_insert(i);
                last = i;
                occurrences += 1;
            }
            for &start in starts {
                // Only visit the start of each contiguous match on this alignment.
                if i > 0 && start > 0 && chunk.tokens[i - 1] == self.words[start - 1] {
                    continue;
                }
                let len = chunk.tokens[i..]
                    .iter()
                    .zip(&self.words[start..])
                    .take_while(|(a, b)| a == b)
                    .count();
                longest = longest.max(len);
                if let Some(strengths) = self.quotes.get(start) {
                    quote = quote.max(strengths[len - 1]);
                }
            }
        }
        let Some(first) = first else {
            return LexicalMatch::default();
        };
        let coverage = matched_idf / self.total_idf;
        let phrase = longest.min(12) as f64 / self.words.len().min(12) as f64;
        let proximity = if occurrences > 1 {
            self.idf.len().min(occurrences) as f64 / (last - first + 1) as f64
        } else {
            0.
        };
        LexicalMatch {
            score: (0.65 * coverage + 0.3 * phrase + 0.05 * proximity).min(1.),
            coverage,
            quote,
        }
    }

    fn semantic_weight(&self, quote_confidence: f64, best_coverage: f64) -> f64 {
        if self.informative_len == 0 {
            return 0.30;
        }
        // A long prompt that already matches most words in one passage still
        // behaves like a quote. Only its unexplained length earns the small boost.
        let unmatched = self.informative_len as f64 * (1. - best_coverage.clamp(0., 1.));
        let length = ((unmatched - 3.) / 9.).clamp(0., 1.);
        let without_quote = 0.80 + 0.04 * length;
        without_quote + quote_confidence.clamp(0., 1.) * (0.25 - without_quote)
    }
}

fn rank(corpus: &Corpus, query: &str, vector: Option<&[f32]>) -> Vec<Hit> {
    let query_tokens = tokens(query);
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let unique_chunks: Vec<_> = corpus
        .docs
        .values()
        .flat_map(|d| &d.chunks)
        .map(|c| (&c.hash, c))
        .collect::<HashMap<_, _>>()
        .into_values()
        .collect();
    let terms = QueryTerms::new(&query_tokens, &unique_chunks, vector.is_some());
    let vectors = corpus.vectors.read().unwrap_or_else(|e| e.into_inner());
    let mut candidates = Vec::new();
    let mut quote_confidence: f64 = 0.;
    let mut best_coverage: f64 = 0.;
    for chunk in unique_chunks {
        let lexical = terms.lexical_match(chunk);
        let semantic = vector.and_then(|q| {
            vectors.get(&chunk.hash).map(|v| {
                q.iter()
                    .zip(v.iter())
                    .map(|(a, b)| (*a as f64) * (*b as f64))
                    .sum::<f64>()
                    .clamp(-1., 1.)
            })
        });
        if lexical.score > 0.12 || semantic.is_some_and(|s| s >= 0.78) {
            quote_confidence = quote_confidence.max(lexical.quote * lexical.coverage);
            best_coverage = best_coverage.max(lexical.coverage);
            candidates.push((chunk.hash.as_str(), lexical, semantic));
        }
    }
    drop(vectors);
    let weight = terms.semantic_weight(quote_confidence, best_coverage);
    let scores: HashMap<_, _> = candidates
        .into_iter()
        .map(|(hash, lexical, cosine)| {
            let blended = cosine.map_or(lexical.score, |s| {
                let dense = ((s - 0.65) / 0.30).clamp(0., 1.);
                (1. - weight) * lexical.score + weight * dense + 0.15 * lexical.score * dense
            });
            let quote_bonus = if vector.is_some() {
                0.25 * lexical.quote
            } else {
                0.
            };
            (hash, (blended + quote_bonus, lexical.score))
        })
        .collect();
    let mut hits = Vec::new();
    for doc in corpus.docs.values() {
        let best = doc
            .chunks
            .iter()
            .filter_map(|c| scores.get(c.hash.as_str()))
            .max_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((score, _lexical)) = best {
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
    fn semantic_weight_uses_distinct_content_words_and_the_best_quote() {
        let weight = |text: &str, confidence| {
            let words = tokens(text);
            QueryTerms::new(&words, &[], true).semantic_weight(confidence, 0.)
        };
        let short = "рассвет лодка берег";
        let long = "рассвет лодка берег путник увидел свет башни вернулся домой";
        assert_eq!(
            weight(short, 0.),
            weight("я на берег берег лодка и там рассвет", 0.)
        );
        assert!(weight(long, 0.) > weight(short, 0.));
        assert!(weight(long, 0.) > weight(long, 1.));
        let words = tokens(long);
        let terms = QueryTerms::new(&words, &[], true);
        assert!(terms.semantic_weight(0., 0.) > terms.semantic_weight(0., 0.8));
        let capped = "a b c d e f g h j k l m n o p";
        assert_eq!(weight(capped, 0.5), weight(&format!("{capped} q r s"), 0.5));
    }

    #[test]
    fn direct_evidence_rewards_adjacent_rare_words_and_discounts_repeats() {
        let passages = [
            "серебряный компас",
            "серебряный потерянный компас",
            "летит птица",
            "летит птица далеко",
            "опять летит птица",
            "летит птица домой",
            "эхо эхо эхо",
        ]
        .map(|s| chunks(s).remove(0));
        let chunks = passages.iter().collect::<Vec<_>>();
        let words = tokens("серебряный компас летит птица эхо эхо там где волна");
        let query = QueryTerms::new(&words, &chunks, true);
        let rare = query.lexical_match(&passages[0]).quote;
        let common = query.lexical_match(&passages[2]).quote;
        assert!(rare > common && common > 0.);
        assert_eq!(query.lexical_match(&passages[1]).quote, 0.);
        assert_eq!(query.lexical_match(&passages[6]).quote, 0.);
    }

    #[test]
    fn scattered_hits_allow_semantic_retrieval_and_unindexed_quotes_still_surface() {
        let c = corpus(&[
            "маяк посреди моря",
            "путеводная звезда ведёт домой",
            "горный приют",
        ]);
        let mut q = vec![0.; DIMENSIONS];
        q[0] = 1.;
        let mut passage = vec![0.; DIMENSIONS];
        passage[0] = 0.94;
        let hash = c.docs["1"].chunks[0].hash.clone();
        c.vectors.write().unwrap().insert(hash, Arc::new(passage));
        let recall = "маяк звездочёт возвращение корабля ночью";
        assert_eq!(rank(&c, recall, None)[0].hash, "0");
        assert_eq!(rank(&c, recall, Some(&q))[0].hash, "1");
        for vector in [None, Some(q.as_slice())] {
            assert_eq!(rank(&c, "маяк посреди моря", vector)[0].hash, "0");
            assert!(rank(&c, "…", vector).is_empty());
        }
    }

    #[test]
    fn malformed_vectors_are_rejected() {
        assert!(decode_vector(&[0; 4]).is_none());
        assert!(decode_vector(&vec![0; DIMENSIONS * 4]).is_none());
    }
    #[tokio::test]
    #[ignore = "requires cached E5 model; FURU_LYRICS_MODEL_DIR"]
    async fn real_e5_multilingual_retrieval() {
        let dir = crate::lyrics::model_files::prepare(false).await.unwrap();
        let mut model = load_model(&dir).unwrap();
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

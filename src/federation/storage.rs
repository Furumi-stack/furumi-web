use std::str::FromStr;

use async_trait::async_trait;
use music_dht::{
    DhtKey, EndpointId, LibraryItem, MAX_RECORDS_PER_RESPONSE, MusicDhtError, MusicDhtStorage,
    NodeContact, NodeId, SecretKey, StoreDecision, StoredRecord, decide_store,
};
use sqlx::{PgPool, Row as _};

const IDENTITY_NAME: &str = "default";
// Several republish paths can converge on the same record within seconds
// (startup discovery, a manual sync and a periodic tick). Extending a
// twelve-hour TTL by less than five minutes is operationally identical, but
// rewriting the wide row and its expiry index generates substantial WAL.
const MIN_EXPIRY_EXTENSION_MS: u64 = 5 * 60 * 1_000;

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS furumusic__federation_identity (
        name        TEXT PRIMARY KEY,
        secret_key  BYTEA NOT NULL,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS furumusic__federation_local_item (
        id              BYTEA PRIMARY KEY,
        normalized_name TEXT NOT NULL,
        revision        BIGINT NOT NULL,
        deleted         BOOLEAN NOT NULL DEFAULT false,
        updated_at_ms   BIGINT NOT NULL,
        payload         BYTEA NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_furumusic_federation_local_item_normalized_name
        ON furumusic__federation_local_item(normalized_name)",
    "CREATE TABLE IF NOT EXISTS furumusic__federation_dht_record (
        dht_key          BYTEA NOT NULL,
        item_id          BYTEA NOT NULL,
        owner_peer_id    TEXT NOT NULL,
        payload          BYTEA NOT NULL,
        revision         BIGINT NOT NULL,
        deleted          BOOLEAN NOT NULL,
        expires_at_ms    BIGINT NOT NULL,
        PRIMARY KEY (dht_key, item_id, owner_peer_id)
    )",
    "CREATE INDEX IF NOT EXISTS idx_furumusic_federation_dht_record_expires_at
        ON furumusic__federation_dht_record(expires_at_ms)",
    "CREATE TABLE IF NOT EXISTS furumusic__federation_known_peer (
        peer_id       TEXT PRIMARY KEY,
        node_id       BYTEA NOT NULL,
        ticket        TEXT NOT NULL,
        last_seen_ms  BIGINT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS furumusic__federation_content_id_cache (
        media_file_id  BIGINT PRIMARY KEY,
        sha256_hash    TEXT NOT NULL,
        content_id     TEXT NOT NULL,
        updated_at     TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_furumusic_federation_content_id_cache_content_id
        ON furumusic__federation_content_id_cache(content_id)",
];

#[derive(Debug, Clone)]
pub struct PostgresFederationStorage {
    pool: PgPool,
}

impl PostgresFederationStorage {
    pub async fn new(pool: PgPool) -> music_dht::Result<Self> {
        let storage = Self { pool };
        storage.ensure_schema().await?;
        Ok(storage)
    }

    pub async fn load_or_create_secret_key(&self) -> music_dht::Result<SecretKey> {
        if let Some(bytes) = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT secret_key FROM furumusic__federation_identity WHERE name = $1",
        )
        .bind(IDENTITY_NAME)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?
        {
            return secret_from_bytes(bytes);
        }

        let key = SecretKey::generate();
        let key_bytes = key.to_bytes();
        let now = now_iso();
        let inserted = sqlx::query(
            "INSERT INTO furumusic__federation_identity
             (name, secret_key, created_at, updated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(IDENTITY_NAME)
        .bind(key_bytes.as_slice())
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected();
        if inserted == 1 {
            return Ok(key);
        }

        let bytes = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT secret_key FROM furumusic__federation_identity WHERE name = $1",
        )
        .bind(IDENTITY_NAME)
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)?;
        secret_from_bytes(bytes)
    }

    async fn ensure_schema(&self) -> music_dht::Result<()> {
        for sql in SCHEMA {
            sqlx::query(sql)
                .execute(&self.pool)
                .await
                .map_err(db_error)?;
        }
        Ok(())
    }
}

#[async_trait]
impl MusicDhtStorage for PostgresFederationStorage {
    async fn upsert_local_item(&self, item: &LibraryItem) -> music_dht::Result<()> {
        let payload = postcard::to_stdvec(item).map_err(db_error)?;
        sqlx::query(
            "INSERT INTO furumusic__federation_local_item
             (id, normalized_name, revision, deleted, updated_at_ms, payload)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (id) DO UPDATE SET
                normalized_name = EXCLUDED.normalized_name,
                revision = EXCLUDED.revision,
                deleted = EXCLUDED.deleted,
                updated_at_ms = EXCLUDED.updated_at_ms,
                payload = EXCLUDED.payload
             WHERE (furumusic__federation_local_item.normalized_name,
                    furumusic__federation_local_item.revision,
                    furumusic__federation_local_item.deleted,
                    furumusic__federation_local_item.updated_at_ms,
                    furumusic__federation_local_item.payload)
                   IS DISTINCT FROM
                   (EXCLUDED.normalized_name,
                    EXCLUDED.revision,
                    EXCLUDED.deleted,
                    EXCLUDED.updated_at_ms,
                    EXCLUDED.payload)",
        )
        .bind(item.id.as_bytes().as_slice())
        .bind(&item.normalized_name)
        .bind(item.revision as i64)
        .bind(item.deleted)
        .bind(item.updated_at_ms as i64)
        .bind(payload)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn list_local_items(&self, include_deleted: bool) -> music_dht::Result<Vec<LibraryItem>> {
        let rows = sqlx::query(
            "SELECT payload
             FROM furumusic__federation_local_item
             WHERE $1 OR deleted = false
             ORDER BY normalized_name",
        )
        .bind(include_deleted)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| postcard::from_bytes::<LibraryItem>(&row.get::<Vec<u8>, _>(0)).ok())
            .collect())
    }

    async fn store_dht_record(&self, key: DhtKey, record: StoredRecord) -> music_dht::Result<bool> {
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        store_record_in_conn(&mut conn, &key, &record).await
    }

    async fn store_dht_records(
        &self,
        entries: Vec<(DhtKey, StoredRecord)>,
    ) -> music_dht::Result<Vec<bool>> {
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        let mut stored = Vec::with_capacity(entries.len());
        for (key, record) in &entries {
            stored.push(store_record_in_conn(&mut tx, key, record).await?);
        }
        tx.commit().await.map_err(db_error)?;
        Ok(stored)
    }

    async fn dht_records_by_key(
        &self,
        key: DhtKey,
        now_ms: u64,
    ) -> music_dht::Result<Vec<StoredRecord>> {
        let rows = sqlx::query(
            "SELECT payload
             FROM furumusic__federation_dht_record
             WHERE dht_key = $1 AND expires_at_ms > $2
             ORDER BY expires_at_ms DESC, item_id
             LIMIT $3",
        )
        .bind(key.as_bytes().as_slice())
        .bind(now_ms as i64)
        .bind(MAX_RECORDS_PER_RESPONSE as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| postcard::from_bytes::<StoredRecord>(&row.get::<Vec<u8>, _>(0)).ok())
            .collect())
    }

    async fn delete_expired_records(&self, now_ms: u64) -> music_dht::Result<usize> {
        let result =
            sqlx::query("DELETE FROM furumusic__federation_dht_record WHERE expires_at_ms <= $1")
                .bind(now_ms as i64)
                .execute(&self.pool)
                .await
                .map_err(db_error)?;
        Ok(result.rows_affected() as usize)
    }

    async fn upsert_known_peer(&self, contact: &NodeContact) -> music_dht::Result<()> {
        sqlx::query(
            "INSERT INTO furumusic__federation_known_peer
             (peer_id, node_id, ticket, last_seen_ms)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (peer_id) DO UPDATE SET
                node_id = EXCLUDED.node_id,
                ticket = EXCLUDED.ticket,
                last_seen_ms = EXCLUDED.last_seen_ms",
        )
        .bind(contact.peer_id.to_string())
        .bind(contact.node_id.as_bytes().as_slice())
        .bind(&contact.ticket)
        .bind(contact.last_seen_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn delete_known_peer(&self, peer_id: EndpointId) -> music_dht::Result<()> {
        sqlx::query("DELETE FROM furumusic__federation_known_peer WHERE peer_id = $1")
            .bind(peer_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn load_known_peers(&self) -> music_dht::Result<Vec<NodeContact>> {
        let rows = sqlx::query(
            "SELECT peer_id, node_id, ticket, last_seen_ms
             FROM furumusic__federation_known_peer",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        let mut contacts = Vec::new();
        for row in rows {
            let peer_id: String = row.get(0);
            let node_id: Vec<u8> = row.get(1);
            let ticket: String = row.get(2);
            let last_seen_ms: i64 = row.get(3);
            let Ok(peer_id) = EndpointId::from_str(&peer_id) else {
                continue;
            };
            let Ok(node_id) = <[u8; 32]>::try_from(node_id.as_slice()) else {
                continue;
            };
            contacts.push(NodeContact {
                node_id: NodeId::from_bytes(node_id),
                peer_id,
                ticket,
                last_seen_ms: last_seen_ms as u64,
            });
        }
        Ok(contacts)
    }
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn secret_from_bytes(bytes: Vec<u8>) -> music_dht::Result<SecretKey> {
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| MusicDhtError::Database("stored federation identity is corrupted".into()))?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// Applies one validated record following the revision/tombstone rules.
/// Returns `true` if the record was written or refreshed. Runs against a
/// pooled connection or an open transaction.
async fn store_record_in_conn(
    conn: &mut sqlx::PgConnection,
    key: &DhtKey,
    record: &StoredRecord,
) -> music_dht::Result<bool> {
    let existing = sqlx::query(
        "SELECT revision, deleted, expires_at_ms
         FROM furumusic__federation_dht_record
         WHERE dht_key = $1 AND item_id = $2 AND owner_peer_id = $3",
    )
    .bind(key.as_bytes().as_slice())
    .bind(record.item.id.as_bytes().as_slice())
    .bind(record.item.owner.to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)?
    .map(|row| {
        (
            row.get::<i64, _>(0) as u64,
            row.get::<bool, _>(1),
            row.get::<i64, _>(2) as u64,
        )
    });

    match decide_store(existing, record) {
        StoreDecision::Ignore => return Ok(false),
        StoreDecision::RefreshExpiry(new_expiry) => {
            let previous_expiry = existing.map(|(_, _, expiry)| expiry).unwrap_or(0);
            if new_expiry.saturating_sub(previous_expiry) < MIN_EXPIRY_EXTENSION_MS {
                return Ok(false);
            }
        }
        StoreDecision::Write => {}
    }

    let payload = postcard::to_stdvec(record).map_err(db_error)?;
    let written = sqlx::query(
        "INSERT INTO furumusic__federation_dht_record
         (dht_key, item_id, owner_peer_id, payload, revision, deleted, expires_at_ms)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (dht_key, item_id, owner_peer_id) DO UPDATE SET
            payload = EXCLUDED.payload,
            revision = EXCLUDED.revision,
            deleted = EXCLUDED.deleted,
            expires_at_ms = EXCLUDED.expires_at_ms
         WHERE (furumusic__federation_dht_record.payload,
                furumusic__federation_dht_record.revision,
                furumusic__federation_dht_record.deleted,
                furumusic__federation_dht_record.expires_at_ms)
               IS DISTINCT FROM
               (EXCLUDED.payload,
                EXCLUDED.revision,
                EXCLUDED.deleted,
                EXCLUDED.expires_at_ms)",
    )
    .bind(key.as_bytes().as_slice())
    .bind(record.item.id.as_bytes().as_slice())
    .bind(record.item.owner.to_string())
    .bind(payload)
    .bind(record.item.revision as i64)
    .bind(record.item.deleted)
    .bind(record.expires_at_ms as i64)
    .execute(&mut *conn)
    .await
    .map_err(db_error)?
    .rows_affected();
    Ok(written > 0)
}

fn db_error(err: impl std::fmt::Display) -> MusicDhtError {
    MusicDhtError::Database(err.to_string())
}

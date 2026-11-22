CREATE TABLE furumusic__lyrics (
    audio_sha256 TEXT NOT NULL,
    id TEXT NOT NULL,
    label TEXT NOT NULL,
    source TEXT NOT NULL,
    source_id TEXT,
    language TEXT,
    plain_text TEXT,
    synced_lrc TEXT,
    instrumental BOOLEAN NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (audio_sha256, id)
);
CREATE TABLE furumusic__lyrics_lookup (
    signature TEXT PRIMARY KEY,
    status TEXT NOT NULL CHECK (status IN ('found', 'not_found', 'retry')),
    versions JSONB NOT NULL DEFAULT '[]',
    attempts INTEGER NOT NULL DEFAULT 1,
    retry_at TIMESTAMPTZ,
    checked_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE furumusic__lyrics_binding (
    audio_sha256 TEXT PRIMARY KEY,
    signature TEXT NOT NULL
);
CREATE INDEX idx_lyrics_lookup_retry ON furumusic__lyrics_lookup(retry_at) WHERE status = 'retry';

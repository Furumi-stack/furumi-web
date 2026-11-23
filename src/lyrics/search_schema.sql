CREATE TABLE IF NOT EXISTS furumusic__lyrics_embedding (
    profile TEXT NOT NULL,
    text_hash TEXT NOT NULL,
    vector BYTEA NOT NULL CHECK (octet_length(vector) = 1536),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (profile, text_hash)
);

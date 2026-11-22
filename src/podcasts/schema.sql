CREATE TABLE IF NOT EXISTS furumusic__podcast (
 id TEXT PRIMARY KEY, origin_url TEXT NOT NULL UNIQUE, feed_url TEXT NOT NULL,
 metadata JSONB NOT NULL, etag TEXT, modified TEXT, checked_at TIMESTAMPTZ,
 last_error TEXT, created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS furumusic__podcast_alias (
 url TEXT PRIMARY KEY, podcast_id TEXT NOT NULL REFERENCES furumusic__podcast(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS furumusic__podcast_subscription (
 user_id BIGINT NOT NULL REFERENCES furumusic__user(id) ON DELETE CASCADE,
 podcast_id TEXT NOT NULL REFERENCES furumusic__podcast(id) ON DELETE CASCADE,
 shared BOOLEAN NOT NULL DEFAULT false, confirmed_urls JSONB,
 created_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY(user_id,podcast_id)
);
CREATE TABLE IF NOT EXISTS furumusic__podcast_episode (
 id TEXT PRIMARY KEY, podcast_id TEXT NOT NULL REFERENCES furumusic__podcast(id) ON DELETE CASCADE,
 metadata JSONB NOT NULL, discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
 cached_at TIMESTAMPTZ, download_until TIMESTAMPTZ, retry_at TIMESTAMPTZ,
 download_error TEXT, expired BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX IF NOT EXISTS idx_podcast_episode_feed ON furumusic__podcast_episode(podcast_id);
CREATE TABLE IF NOT EXISTS furumusic__podcast_progress (
 user_id BIGINT NOT NULL REFERENCES furumusic__user(id) ON DELETE CASCADE,
 episode_id TEXT NOT NULL REFERENCES furumusic__podcast_episode(id) ON DELETE CASCADE,
 position_ms BIGINT NOT NULL DEFAULT 0, completed BOOLEAN NOT NULL DEFAULT false,
 generation BIGINT NOT NULL DEFAULT 0, checkpoint BIGINT NOT NULL DEFAULT 0,
 updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY(user_id,episode_id)
);
CREATE TABLE IF NOT EXISTS furumusic__podcast_settings (
 id INTEGER PRIMARY KEY CHECK(id=1), retention_days INTEGER CHECK(retention_days BETWEEN 1 AND 36500)
);
INSERT INTO furumusic__podcast_settings(id) VALUES(1) ON CONFLICT DO NOTHING;

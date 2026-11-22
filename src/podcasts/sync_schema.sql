CREATE TABLE IF NOT EXISTS furumusic__podcast_register (
 user_id BIGINT NOT NULL REFERENCES furumusic__user(id) ON DELETE CASCADE,
 kind TEXT NOT NULL, entity_id TEXT NOT NULL, value JSONB NOT NULL,
 PRIMARY KEY(user_id,kind,entity_id)
);
CREATE COLLATION IF NOT EXISTS furumusic_unicode (provider=icu, locale='und', deterministic=true);

ALTER TABLE furumusic__track
    ADD COLUMN IF NOT EXISTS lastfm_checked_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS lastfm_lookup_title TEXT,
    ADD COLUMN IF NOT EXISTS lastfm_lookup_artist TEXT;

CREATE INDEX IF NOT EXISTS idx_track_artist_lastfm_lookup
    ON furumusic__track_artist (track_id, position, artist_id)
    WHERE role <> 'featuring';

-- Preserve existing observations, including valid zero-valued popularity.
-- Legacy misses cannot be backfilled because they were only written to logs.
UPDATE furumusic__track t
   SET lastfm_checked_at = CURRENT_TIMESTAMP,
       lastfm_lookup_title = t.title::text,
       lastfm_lookup_artist = (
           SELECT a.name::text
             FROM furumusic__track_artist ta
             JOIN furumusic__artist a ON a.id = ta.artist_id
            WHERE ta.track_id = t.id AND ta.role <> 'featuring'
            ORDER BY ta.position, ta.artist_id
            LIMIT 1
       )
 WHERE t.lastfm_checked_at IS NULL
   AND (t.lastfm_updated_at IS NOT NULL
    OR t.lastfm_rating IS NOT NULL
    OR t.lastfm_listeners IS NOT NULL
    OR t.lastfm_playcount IS NOT NULL);

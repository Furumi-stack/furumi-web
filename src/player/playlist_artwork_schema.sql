ALTER TABLE furumusic__playlist
    ADD COLUMN cover_ids bigint[] NOT NULL DEFAULT '{}'
        CHECK (cardinality(cover_ids) <= 4),
    ADD COLUMN cover_jpeg bytea,
    ADD COLUMN cover_hash text,
    ADD CONSTRAINT playlist_cover_image_pair CHECK ((cover_jpeg IS NULL) = (cover_hash IS NULL))

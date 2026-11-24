CREATE VIEW furumusic__track_content_alias AS
SELECT t.id AS track_id, c.content_id, (t.is_hidden OR r.is_hidden) AS hidden
FROM furumusic__federation_content_id_cache c
JOIN furumusic__media_file m ON m.id = c.media_file_id AND m.sha256_hash = c.sha256_hash
JOIN furumusic__track t ON t.audio_file_id = m.id
JOIN furumusic__release r ON r.id = t.release_id

-- The web history endpoint generated this four-part ID for older clients.
-- Imports from the old play_history table use the shorter legacy-web:<id>.
UPDATE furumusic__listen_event
   SET listened_ms = track_duration_ms
 WHERE track_duration_ms > 0
   AND listened_ms > track_duration_ms
   AND listen_id ~ '^legacy-web:[0-9]+:[0-9]+:[0-9]+$'

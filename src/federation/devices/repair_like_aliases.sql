-- A local click inserted its selected track before device sync chose another
-- copy of the same audio. Keep the first visible insertion for each listener.
WITH ranked AS (
    SELECT l.id, row_number() OVER (
        PARTITION BY l.user_id, a.content_id ORDER BY a.hidden, l.id
    ) AS position
    FROM furumusic__user_liked_track l
    JOIN furumusic__track_content_alias a ON a.track_id = l.track_id
)
DELETE FROM furumusic__user_liked_track l USING ranked
WHERE l.id = ranked.id AND ranked.position > 1

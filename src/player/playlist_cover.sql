SELECT CASE
  WHEN p.cover_hash IS NOT NULL THEN ARRAY['/api/player/playlists/' || p.id || '/cover?v=' || p.cover_hash]
  WHEN p.cover_file_id IS NOT NULL THEN ARRAY['/api/player/cover/' || p.cover_file_id || '/small']
  ELSE (
    SELECT COALESCE((array_agg(url ORDER BY choice) FILTER (WHERE choice IS NOT NULL))[1:4],
                    (array_agg(url ORDER BY position, item_id) FILTER (WHERE automatic))[1:4], ARRAY[]::text[])
    FROM (
      SELECT '/api/player/cover/' || art || '/small' AS url,
             array_position(p.cover_ids, art) AS choice, position, item_id, automatic
        FROM ({candidates}) options
    ) ranked
  ) END AS urls

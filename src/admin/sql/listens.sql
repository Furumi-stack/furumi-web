WITH listens AS (
    SELECT le.*, COALESCE(NULLIF(u.display_name, ''), u.username::text, 'Deleted user #' || le.user_id) AS user_name,
           COALESCE(NULLIF(le.metadata_json->>'title', ''), 'Unknown track') AS title,
           COALESCE(le.metadata_json->>'release_title', '') AS release_title,
           ARRAY(SELECT DISTINCT trim(name) FROM jsonb_array_elements_text(
               COALESCE(le.metadata_json->'artist_names', '[]'::jsonb) ||
               COALESCE(le.metadata_json->'featured_artist_names', '[]'::jsonb)
           ) AS names(name) WHERE trim(name) <> '' ORDER BY trim(name)) AS artist_names,
           CASE WHEN le.origin_device_id = ident.device_id THEN 'instance' ELSE 'synced' END AS source,
           COALESCE(NULLIF(d.name, ''), le.origin_device_id) AS device_name
    FROM furumusic__listen_event le
    LEFT JOIN furumusic__user u ON u.id = le.user_id
    LEFT JOIN furumusic__fed_device_identity ident ON ident.user_id = le.user_id
    LEFT JOIN furumusic__fed_device d ON d.user_id = le.user_id AND d.device_id = le.origin_device_id
    WHERE le.started_at_ms >= $1 AND le.started_at_ms < $2
      AND ($3::bigint IS NULL OR le.user_id = $3)
      AND ($4::text IS NULL OR le.content_id = $4)
), filtered AS (
    SELECT * FROM listens
    WHERE ($5::text IS NULL OR EXISTS (SELECT 1 FROM unnest(artist_names) n WHERE lower(n) = lower($5)))
      AND ($6 = 'all' OR source = $6)
)

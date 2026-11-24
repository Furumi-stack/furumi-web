WITH wanted AS (
    SELECT * FROM unnest($2::text[],$3::text[]) WITH ORDINALITY w(source,id,position)
    WHERE NOT EXISTS (SELECT 1 FROM furumusic__hidden_download h
          WHERE h.user_id=$1 AND h.source=w.source AND h.download_id=w.id)
), jobs AS (
    SELECT w.position,w.source,w.id,
           CASE WHEN count(*)=1 THEN min(regexp_replace(f.filename,'^.*/',''))
                WHEN bool_and(strpos(f.filename,'/')>0)
                    AND min(split_part(f.filename,'/',1))=max(split_part(f.filename,'/',1))
                THEN min(split_part(f.filename,'/',1)) END AS title,
           CASE WHEN bool_or(f.status='uploading') THEN 'uploading'
                WHEN bool_or(f.status='ai_processing') THEN 'ai_processing'
                WHEN bool_or(f.status='queued') THEN 'queued'
                WHEN bool_or(f.status='needs_review') THEN 'needs_review'
                WHEN bool_and(f.status='failed') THEN 'failed'
                WHEN bool_and(f.status='cancelled') THEN 'cancelled'
                WHEN bool_or(f.status IN ('failed','cancelled')) THEN 'complete_with_errors'
                ELSE 'complete' END AS status,
           min(f.created_at) AS created_at, max(f.updated_at) AS updated_at, count(*) AS item_count,
           CASE WHEN count(*)>1 THEN 100.0::float8 * count(*) FILTER (WHERE f.status='complete') / count(*) END AS progress_percent,
           min(f.error) FILTER (WHERE f.status='failed') AS error, false AS processed
    FROM wanted w JOIN (
        SELECT batch_id,replace(filename,chr(92),'/') AS filename,status,created_at,updated_at,error
        FROM furumusic__local_upload WHERE user_id=$1
    ) f ON w.source='files' AND w.id=f.batch_id
    GROUP BY w.position,w.source,w.id
    UNION ALL
    SELECT w.position,w.source,w.id,t.name,
           CASE WHEN review.processing THEN 'ai_processing'
                WHEN review.queued THEN 'awaiting_ai'
                WHEN review.failed THEN 'ai_failed'
                WHEN review.pending THEN 'needs_review'
                WHEN review.cancelled THEN 'cancelled'
                WHEN review.partly_cancelled THEN 'complete_with_errors'
                ELSE t.status END,
           t.created_at,t.updated_at,t.history_item_count::bigint,
           CASE WHEN t.status='downloading' THEN t.progress_percent END,
           coalesce(t.error,review.error), t.history_processed
    FROM wanted w JOIN furumusic__torrent_session t ON w.source='torrents' AND w.id=t.id AND t.user_id=$1
    LEFT JOIN LATERAL (
        SELECT bool_or(r.status='processing') AS processing, bool_or(r.status='queued') AS queued,
               bool_or(r.status='failed') AS failed, bool_or(r.status IN ('rejected','pending')) AS pending,
               bool_and(r.status='cancelled') AS cancelled, bool_or(r.status='cancelled') AS partly_cancelled,
               min(r.error_message) FILTER (WHERE r.status='failed') AS error
        FROM furumusic__upload_review_state r WHERE t.status='complete' AND NOT t.history_processed
          AND split_part(r.input_path,'/',1)='user_uploads' AND split_part(r.input_path,'/',2)=t.user_id::text
          AND split_part(r.input_path,'/',3)='torrent-'||t.id
    ) review ON true
    UNION ALL
    SELECT w.position,w.source,w.id,coalesce(nullif(y.title,''),y.source_url),y.status,
           y.created_at,y.updated_at,y.total_items::bigint,
           CASE WHEN y.status='downloading' THEN
               (SELECT avg(i.progress_percent) FROM furumusic__youtube_download_item i WHERE i.job_id=y.id) END,
           y.error,false
    FROM wanted w JOIN furumusic__youtube_download y ON w.source='youtube' AND w.id=y.id AND y.user_id=$1
)
SELECT source,id,title,status,created_at,updated_at,item_count,progress_percent,error,processed
FROM jobs ORDER BY position

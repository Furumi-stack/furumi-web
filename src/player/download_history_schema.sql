-- Older clients store the true upload batch in its server-assigned inbox directory.
ALTER TABLE furumusic__local_upload ADD COLUMN IF NOT EXISTS batch_id text GENERATED ALWAYS AS (
    COALESCE(substring(replace(inbox_path, chr(92), '/') from
        '^user_uploads/[0-9]+/local-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/'), id)
) STORED;
CREATE INDEX IF NOT EXISTS idx_local_upload_batch ON furumusic__local_upload(user_id,batch_id,created_at);
ALTER TABLE furumusic__torrent_session ADD COLUMN IF NOT EXISTS history_item_count integer GENERATED ALWAYS AS (
    CASE WHEN selected_files_json = '[]' THEN json_array_length(files_json::json)
         ELSE json_array_length(selected_files_json::json) END
) STORED;
ALTER TABLE furumusic__torrent_session ADD COLUMN IF NOT EXISTS history_processed boolean NOT NULL DEFAULT false;
CREATE INDEX IF NOT EXISTS idx_pending_review_upload_directory ON furumusic__pending_review
    ((split_part(input_path,'/',2)), (split_part(input_path,'/',3)))
    WHERE split_part(input_path,'/',1)='user_uploads';
CREATE TABLE IF NOT EXISTS furumusic__hidden_download (
    user_id bigint NOT NULL, source text NOT NULL, download_id text NOT NULL,
    PRIMARY KEY (user_id, source, download_id)
);

SELECT COALESCE(t.cover_file_id, album.cover_file_id) FROM furumusic__playlist_track pt
JOIN furumusic__track t ON t.id = pt.track_id AND NOT t.is_hidden
JOIN furumusic__release album ON album.id = t.release_id AND NOT album.is_hidden
WHERE pt.playlist_id = p.id AND COALESCE(t.cover_file_id, album.cover_file_id) IS NOT NULL
ORDER BY pt.position, pt.id LIMIT 1

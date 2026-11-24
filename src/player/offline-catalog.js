/* Shared by the player and its service worker. A track becomes visible only after
 * the complete OPFS file is closed and its IndexedDB transaction commits. */
(() => {
    'use strict';
    const BASE = '/api/player/';
    const DEFAULT_LIMIT = 3_000_000_000;
    const MAX_LIMIT = 512_000_000_000;
    const stores = ['tracks', 'files', 'assets', 'meta', 'plays', 'lyrics', 'pending'];
    const requests = request => new Promise((resolve, reject) => {
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
    });
    const validId = value => Number.isSafeInteger(value) && value > 0;
    const positiveId = value => validId(Number(value)) ? Number(value) : null;
    // Server IDs are sequential. Keep browser-only IDs in the upper half of the safe integer range.
    const syntheticBase = 2 ** 52;
    const normalized = value => String(value || '').normalize('NFKC').trim().toLowerCase();
    const contentToken = id => typeof id === 'string' && /^b3:[a-f\d]{64}$/.test(id) ? id.slice(3) : null;
    const remoteMedia = id => BASE + 'federation/cache/' + contentToken(id);
    function trackId(value) {
        const id = value?.id ?? value;
        const token = typeof id === 'string' && id.startsWith('fed:') && contentToken(id.slice(4));
        return token ? syntheticBase + parseInt(token.slice(0,13),16) : positiveId(id);
    }
    async function metadataId(type, key) {
        const hash = new DataView(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(JSON.stringify([type,key]))));
        return syntheticBase + (hash.getUint32(0) & 0xfffff) * 2 ** 32 + hash.getUint32(4);
    }
    function preparedRemote(t) {
        return !!contentToken(t?.content_id) && t.id === 'fed:' + t.content_id &&
            t.federated_cache === true && t.stream_url === remoteMedia(t.content_id);
    }
    async function remoteDescriptor(t) {
        const metadata = t._federationTrack?.metadata || {};
        const credit = async a => {
            const name = (typeof a === 'string' ? a : a?.name) || 'Unknown artist';
            const key = normalized(a?.key?.normalized_name || name);
            const id = positiveId(a?.local_id ?? a?.id) || await metadataId('artist',key);
            return {id,name};
        };
        const primary = t.artists?.length ? t.artists : ['Unknown artist'];
        const artists = await Promise.all(primary.map(credit));
        const featured = await Promise.all((t.featured_artists || []).map(credit));
        const key = metadata.release?.key || t.release_key || {};
        const releaseTitle = t.release_title || metadata.release?.title || t.title;
        const year = t.release_year ?? key.year ?? null;
        const names = key.primary_artists?.length ? key.primary_artists : artists.map(a => a.name);
        const releaseArtists = await Promise.all(names.map(name => artists.find(a => normalized(a.name) === normalized(name)) || credit(name)));
        const releaseId = positiveId(t.release_id || metadata.release?.local_id) || await metadataId('release',[
            normalized(key.normalized_title || releaseTitle), names.map(normalized).sort(), year, key.release_type || '',
        ]);
        const id = trackId(t);
        return {id, content_id:t.content_id, title:t.title, artists, featured_artists:featured,
            release_id:releaseId, release_title:releaseTitle, release_year:year, track_number:t.track_number ?? null,
            disc_number:t.disc_number ?? null, duration_seconds:t.duration_seconds || 0, cover_url:coverKey(t.cover_url),
            audio_format:t.audio_format ?? metadata.audio_format, audio_bitrate:t.audio_bitrate ?? metadata.audio_bitrate,
            audio_sample_rate:t.audio_sample_rate ?? metadata.audio_sample_rate, audio_bit_depth:t.audio_bit_depth ?? metadata.audio_bit_depth,
            uploader_name:'Federation', offline_remote:true, has_lyrics:false, stream_url:BASE + 'stream/' + id,
            offline_release:{artists:releaseArtists,release_type:key.release_type || ''}};
    }
    const quotaError = () => new DOMException('Offline cache limit reached', 'QuotaExceededError');
    const trackMissing = () => new DOMException('Recording is no longer available', 'NotFoundError');
    const json = (data, status = 200) => Response.json(data, {status});
    const missing = () => json({error:'Unavailable offline'}, 404);
    function coverKey(url) {
        if (typeof url !== 'string') return null;
        // Serve the saved thumbnail for all resolutions of the same artwork.
        const match = url.match(/^\/api\/player\/cover\/(\d+)(?:\/(?:small|medium|large))?$/);
        if (match) return BASE + 'cover/' + match[1] + '/medium';
        return /^\/api\/player\/playlists\/\d+\/cover\?v=[a-f\d]+$/i.test(url) ? url : null;
    }
    function releaseInfo(release, track) {
        return {
            id: track.release_id, title: release?.title || track.release_title,
            year: release?.year ?? track.release_year, release_type: release?.release_type || '',
            artists: (release?.artists || track.artists || []).map(({id,name}) => ({id,name})),
            cover_url: release?.cover_url || track.cover_url,
            uploaders: (release?.uploaders || []).map(({name,track_count}) => ({name,track_count})),
            track_count: release?.tracks?.length ?? release?.track_count,
        };
    }
    function validTrack(track) {
        return track && validId(track.id) && validId(track.release_id) && !!track.title &&
            Array.isArray(track.artists) && track.stream_url === BASE + 'stream/' + track.id;
    }
    function contentKey(track) {
        return /^b3:[\da-f]{64}$/.test(track.content_id) ? track.content_id : 'track:' + track.id;
    }
    function binary(file, request, mime) {
        const headers = new Headers({'Content-Type':mime || file.type || 'application/octet-stream', 'Accept-Ranges':'bytes'});
        const range = request.headers.get('Range');
        let start = 0, end = file.size - 1;
        if (range) {
            const parts = /^bytes=(\d*)-(\d*)$/i.exec(range);
            if (parts && (parts[1] || parts[2])) {
                if (parts[1]) {
                    start = Number(parts[1]);
                    if (parts[2]) end = Math.min(end, Number(parts[2]));
                } else {
                    start = Math.max(0, file.size - Number(parts[2]));
                }
            } else {
                start = file.size;
            }
            if (!Number.isSafeInteger(start) || start > end || start >= file.size) {
                headers.set('Content-Range', 'bytes */' + file.size);
                return new Response(null, {status:416, headers});
            }
            headers.set('Content-Range', 'bytes ' + start + '-' + end + '/' + file.size);
        }
        headers.set('Content-Length', String(end - start + 1));
        const body = request.method === 'HEAD' ? null : range ? file.slice(start, end + 1) : file;
        return new Response(body, {status:range ? 206 : 200, headers});
    }
    function summarizePlaylist(pl) {
        const {id, title, kind, is_own, is_saved, is_public, owner_name, description, cover_url, cover_urls} = pl;
        return {id, title, kind, is_own, is_saved, is_public, owner_name, description, cover_url, cover_urls};
    }

    class OfflineCatalog {
        static trackId(value) { return trackId(value); }
        static canSave(track) {
            return !!track && !track.podcast && !track.federation_pending && (positiveId(track.id) !== null || preparedRemote(track));
        }
        constructor(account) {
            if (!/^\d+$/.test(String(account)) || !positiveId(account)) throw new TypeError('Missing offline account');
            this.name = 'furumusic-offline-v1-' + Number(account);
            this.account = Number(account);
            this._database = null;
            this._reconciled = false;
        }
        async db() {
            if (!this._database) {
                const opening = indexedDB.open(this.name, 1);
                opening.onupgradeneeded = () => {
                    const db = opening.result;
                    db.createObjectStore('tracks', {keyPath:'id'}).createIndex('file', 'file');
                    for (const name of ['files','assets','meta','plays','lyrics']) db.createObjectStore(name);
                    db.createObjectStore('pending', {keyPath:'id'}).createIndex('queued','queued');
                };
                this._database = requests(opening).then(db => {
                    db.onversionchange = () => { db.close(); this._database = null; };
                    return db;
                }).catch(error => { this._database = null; throw error; });
            }
            return this._database;
        }
        async read(store, key) {
            const tx = (await this.db()).transaction(store);
            return requests(tx.objectStore(store).get(key));
        }
        async all(store) {
            const tx = (await this.db()).transaction(store);
            return requests(tx.objectStore(store).getAll());
        }
        async write(names, work) {
            const tx = (await this.db()).transaction(names, 'readwrite');
            const finished = new Promise((resolve,reject) => {
                tx.oncomplete = resolve;
                tx.onabort = () => reject(tx.error || new DOMException('Database transaction aborted','AbortError'));
            });
            try {
                work(name => tx.objectStore(name));
            } catch (error) {
                tx.abort();
                await finished.catch(() => {});
                throw error;
            }
            await finished;
        }
        async directory() {
            return (await navigator.storage.getDirectory()).getDirectoryHandle(this.name, {create:true});
        }
        async stats() {
            const tx = (await this.db()).transaction(['tracks','meta']);
            const [tracks, bytes] = await Promise.all([requests(tx.objectStore('tracks').count()), requests(tx.objectStore('meta').get('bytes'))]);
            return {tracks, bytes:bytes || 0};
        }
        async list() {
            const tx = (await this.db()).transaction(['tracks','plays','files']);
            const files = tx.objectStore('files');
            const [rows, plays, fileKeys, fileInfo] = await Promise.all([
                requests(tx.objectStore('tracks').getAll()), requests(tx.objectStore('plays').getAll()),
                requests(files.getAllKeys()), requests(files.getAll()),
            ]);
            const mime = new Map(fileKeys.map((key,i) => [key, fileInfo[i].mime]));
            const counts = new Map(plays.map(p => [p.id, p]));
            return rows.map(r => ({...r.track, bytes:r.bytes, audio_mime:mime.get(r.file), downloaded_at:r.downloaded_at,
                last_played_at:counts.get(r.id)?.last_played_at || null, play_count:counts.get(r.id)?.play_count || 0}));
        }
        async has(value) {
            const id = trackId(value);
            return id !== null && Boolean(await this.read('tracks',id));
        }
        async enqueue(track) {
            if (!OfflineCatalog.canSave(track)) return;
            const id = trackId(track);
            const entry = preparedRemote(track) ? await remoteDescriptor(track) : {id, content_id:track.content_id || null};
            await this.write(['pending','tracks'], get => {
                const tracks = get('tracks'), queue = get('pending');
                const existing = tracks.get(id);
                existing.onsuccess = () => {
                    if (existing.result && (!track.content_id || existing.result.track.content_id === track.content_id)) return;
                    const item = queue.get(id);
                    item.onsuccess = () => queue.put({...entry, queued:item.result?.queued ?? Date.now()});
                };
            });
        }
        async pending() {
            const tx = (await this.db()).transaction('pending');
            return requests(tx.objectStore('pending').index('queued').getAll());
        }
        async dequeue(value) {
            const id = trackId(value);
            if (id) await this.write(['pending'], get => get('pending').delete(id));
        }
        async touch(track) {
            const id = trackId(track);
            if (!id) return;
            await this.write(['plays'], get => {
                const store = get('plays'), req = store.get(id);
                req.onsuccess = () => store.put({id, last_played_at:Date.now(), play_count:(req.result?.play_count || 0)+1}, id);
            });
        }
        async observe(raw, value) {
            const url = new URL(raw, location.origin);
            if (url.origin !== location.origin || value == null) return;
            const route = url.pathname.slice(BASE.length);
            let key = route, data;
            if (!url.pathname.startsWith(BASE)) return;
            if (route === 'me' && Number(value.id) === this.account) data = value;
            else if (['likes','follows','pins','state'].includes(route)) data = value;
            else if (route === 'playlists' && Array.isArray(value)) data = value.map(summarizePlaylist);
            else if (/^playlists\/-?\d+$/.test(route) && Array.isArray(value.tracks)) {
                data = {...summarizePlaylist(value), members:value.tracks.map(t => ({id:t.id, content_id:t.content_id,
                    playlist_track_id:t.playlist_track_id, sort_key:t.sort_key}))};
            } else if (route === 'artists' && Array.isArray(value.items)) {
                // Only the compact image references are needed to hydrate a track saved later.
                const mine = url.searchParams.get('mine') === 'true';
                return this.write(['meta'], get => {
                    for (const a of value.items) {
                        if (a.image_url) get('meta').put(a.image_url, 'artist-image/' + a.id);
                        if (mine) get('meta').put(true, 'mine/' + a.id);
                    }
                });
            } else if (/^artists\/\d+$/.test(route) && value.image_url) {
                key = 'artist-image/' + value.id;
                data = value.image_url;
            } else return;
            // In the window, snapshots may come from Alpine reactive proxies.
            const plain = JSON.parse(JSON.stringify(data));
            await this.write(['meta'], get => get('meta').put(plain, key));
        }
        async save(track, options = {}) {
            if (!OfflineCatalog.canSave(track)) throw new TypeError('This track is not available to save');
            const id = trackId(track);
            if (preparedRemote(track)) track = await remoteDescriptor(track);
            return navigator.locks.request(this.name, {signal:options.signal}, () => this._save(id, track, options));
        }
        async _save(id, input, options) {
            const {signal} = options;
            const fetcher = options.fetch || fetch;
            const limit = Math.min(MAX_LIMIT, options.limitBytes ?? DEFAULT_LIMIT);
            if (!Number.isFinite(limit) || limit <= 0) throw quotaError();
            const get = (path, init) => fetcher(path, {...init, signal, cache:'no-store', credentials:'same-origin', redirect:'error'});
            const dir = await this.directory();
            if (!this._reconciled) {
                await this.prune(dir);
                this._reconciled = true;
            }
            if (options.playlist?.tracks) await this.observe(BASE+'playlists/'+options.playlist.id,options.playlist);
            const previous = await this.read('tracks', id);
            if (previous && (!input.content_id || previous.track.content_id === input.content_id)) return this.dequeue(id);
            if (input.offline_remote && (id !== trackId('fed:'+input.content_id) || previous)) throw new Error('Conflicting offline recording ID');

            let track = input;
            if (!validTrack(track) || !Object.hasOwn(track,'content_id') || typeof track.has_lyrics !== 'boolean') {
                const res = await get(BASE + 'tracks-by-ids', {method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({ids:[id]})});
                if ([404,410].includes(res.status)) throw trackMissing();
                if (!res.ok) throw new Error('Cannot load track metadata ('+res.status+')');
                track = (await res.json()).find(t => t.id === id);
                if (!track) throw trackMissing();
            }
            if (!validTrack(track)) throw new Error('Incomplete offline track metadata');
            // Strip transient player state, federated URLs and Alpine proxies.
            track = JSON.parse(JSON.stringify(track));
            const remote = track.offline_remote, remoteRelease = track.offline_release;
            delete track.offline_release;
            delete track.queued;
            const fileKey = contentKey(track);
            let file = await this.read('files', fileKey);
            const used = (await this.stats()).bytes;
            let remaining = limit - used;
            let newName;
            let addedBytes = 0;
            try {
                if (!file) {
                    if (remaining <= 0 || track.file_size_bytes > remaining) throw quotaError();
                    const res = await get(remote ? remoteMedia(track.content_id) : track.stream_url);
                    if ([404,410].includes(res.status)) throw trackMissing();
                    // Media elements often receive 206; only a complete fresh download is suitable here.
                    const mime = (res.headers.get('Content-Type') || '').split(';')[0].trim();
                    if (res.status !== 200 || !res.body || res.headers.has('Content-Range') || /text\/|json|xml/i.test(mime)) {
                        throw new Error('Cannot download a complete audio file ('+res.status+')');
                    }
                    const headerLength = res.headers.get('Content-Length');
                    const expected = Number(headerLength) || Number(track.file_size_bytes) || null;
                    if (expected > remaining) throw quotaError();
                    newName = 'audio-' + crypto.randomUUID();
                    const target = await dir.getFileHandle(newName, {create:true});
                    const writer = await target.createWritable();
                    const reader = res.body.getReader();
                    let bytes = 0;
                    try {
                        for (;;) {
                            signal?.throwIfAborted();
                            const {value, done} = await reader.read();
                            if (done) break;
                            bytes += value.byteLength;
                            if (bytes > remaining) throw quotaError();
                            await writer.write(value);
                        }
                        if (!bytes || (expected && expected !== bytes)) throw new Error('Incomplete audio file');
                        await writer.close();
                    } catch (error) {
                        await Promise.allSettled([reader.cancel(),writer.abort()]);
                        throw error;
                    } finally { reader.releaseLock(); }
                    file = {name:newName, bytes, mime};
                    addedBytes += bytes;
                    remaining -= bytes;
                }
                const optional = async path => {
                    try {
                        const res = await get(path);
                        return res.ok ? await res.json() : null;
                    } catch (error) { signal?.throwIfAborted(); return null; }
                };
                const release = releaseInfo(options.release?.id === track.release_id ? options.release :
                    await this.read('meta','release/'+track.release_id) || (track.release_id < syntheticBase ? await optional(BASE+'releases/'+track.release_id) : null) || remoteRelease, track);
                const lyrics = remote ? await optional(BASE+'federation/tracks/lyrics?content_id='+encodeURIComponent(track.content_id)) :
                    track.has_lyrics ? await optional(BASE+'lyrics/'+id) : null;
                track.has_lyrics = Boolean(lyrics?.versions?.length);
                const artistImages = {}, imageUrls = new Set([track.cover_url, release.cover_url]);
                const credits = new Set([...track.artists,...(track.featured_artists || []),...release.artists].map(a => a.id));
                for (const credit of credits) {
                    const url = await this.read('meta', 'artist-image/'+credit);
                    if (url) { artistImages[credit] = url; imageUrls.add(url); }
                }
                const keys = [], assets = new Map();
                for (const url of imageUrls) {
                    const key = coverKey(url);
                    if (!key || keys.includes(key)) continue;
                    if (!(await this.read('assets', key))) {
                        if (remaining <= 0) continue;
                        try {
                            const res = await get(key);
                            if (!res.ok || !res.headers.get('Content-Type')?.startsWith('image/')) continue;
                            const blob = await res.blob();
                            if (!blob.size || blob.size > remaining) continue;
                            assets.set(key,blob);
                            addedBytes += blob.size;
                            remaining -= blob.size;
                        } catch { signal?.throwIfAborted(); continue; }
                    }
                    keys.push(key);
                }
                if (!keys.includes(coverKey(track.cover_url))) track.cover_url = null;
                if (!keys.includes(coverKey(release.cover_url))) release.cover_url = track.cover_url;
                for (const [artist, url] of Object.entries(artistImages)) if (!keys.includes(coverKey(url))) delete artistImages[artist];
                signal?.throwIfAborted();
                const row = {id,file:fileKey,bytes:file.bytes,track,release,assets:keys,artistImages,downloaded_at:Date.now()};
                await this.write(['tracks','files','assets','meta','lyrics','pending'], getStore => {
                    getStore('tracks').put(row);
                    getStore('files').put(file,fileKey);
                    getStore('meta').put(release,'release/'+track.release_id);
                    getStore('meta').put(used + addedBytes,'bytes');
                    getStore('meta').put(crypto.randomUUID(),'revision');
                    if (lyrics) getStore('lyrics').put(lyrics, id);
                    else getStore('lyrics').delete(id);
                    getStore('pending').delete(id);
                    for (const [key,blob] of assets) getStore('assets').put(blob,key);
                });
                newName = null;
                if (previous) await this.prune(dir);
            } finally {
                if (newName) await dir.removeEntry(newName).catch(() => {});
            }
        }
        async prune(dir) {
            const rows = await this.all('tracks');
            const usedFiles = new Set(rows.map(row => row.file));
            const usedAssets = new Set(rows.flatMap(row => row.assets));
            const activeNames = new Set();
            const db = await this.db();
            const tx = db.transaction(['files','assets']);
            const [fileKeys, files, assetKeys, assets] = await Promise.all([
                requests(tx.objectStore('files').getAllKeys()), requests(tx.objectStore('files').getAll()),
                requests(tx.objectStore('assets').getAllKeys()), requests(tx.objectStore('assets').getAll()),
            ]);
            await this.write(['files','assets','meta'], get => {
                let bytes = 0;
                fileKeys.forEach((key,i) => {
                    if (usedFiles.has(key)) { activeNames.add(files[i].name); bytes += files[i].bytes; }
                    else get('files').delete(key);
                });
                assetKeys.forEach((key,i) => {
                    if (usedAssets.has(key)) bytes += assets[i].size;
                    else get('assets').delete(key);
                });
                get('meta').put(bytes,'bytes');
            });
            for await (const [name] of dir.entries()) {
                if (!activeNames.has(name)) await dir.removeEntry(name);
            }
        }
        previewCleanup(criteria = {}) {
            return this._cleanup(criteria, null);
        }
        removeMany(ids, criteria = {}) {
            if (!Array.isArray(ids)) throw new TypeError('Expected selected track IDs');
            const selected = new Set(ids.map(trackId).filter(Boolean));
            return navigator.locks.request(this.name, () => this._cleanup(criteria, selected));
        }
        async remove(value) {
            const id = trackId(value);
            if (id) await navigator.locks.request(this.name, () => this._cleanup({}, new Set([id]), true));
        }
        async _cleanup(criteria, selected, cancelUnstored = false) {
            for (const [field, cutoff] of Object.entries(criteria)) {
                if (!['downloadedBefore','lastPlayedBefore'].includes(field) || !Number.isFinite(cutoff))
                    throw new TypeError('Invalid offline cleanup cutoff');
            }
            const deleting = selected !== null;
            const db = await this.db();
            const tx = db.transaction(deleting ? stores : ['tracks','plays','files','assets'], deleting ? 'readwrite' : 'readonly');
            const get = name => tx.objectStore(name);
            const finished = new Promise((resolve,reject) => {
                tx.oncomplete = resolve;
                tx.onabort = () => reject(tx.error || new DOMException('Cleanup aborted','AbortError'));
            });
            let ids, bytes, discardedFiles;
            try {
                const [rows, activity, used] = await Promise.all([
                    requests(get('tracks').getAll()), requests(get('plays').getAll()),
                    deleting ? requests(get('meta').get('bytes')) : 0,
                ]);
                const lastPlay = new Map(activity.map(p => [p.id, p.last_played_at]));
                const retainedFiles = new Set(), retainedAssets = new Set(), retainedReleases = new Set();
                const files = new Set(), assets = new Set(), releases = new Set();
                ids = [];
                for (const row of rows) {
                    const lastUsed = lastPlay.get(row.id) ?? row.downloaded_at;
                    const matches = (!deleting || selected.has(row.id))
                        && (criteria.downloadedBefore === undefined || row.downloaded_at < criteria.downloadedBefore)
                        && (criteria.lastPlayedBefore === undefined || lastUsed < criteria.lastPlayedBefore);
                    if (matches) ids.push(row.id);
                    (matches ? files : retainedFiles).add(row.file);
                    (matches ? releases : retainedReleases).add(row.track.release_id);
                    for (const key of row.assets) (matches ? assets : retainedAssets).add(key);
                }
                const fileKeys = [...files].filter(key => !retainedFiles.has(key));
                const assetKeys = [...assets].filter(key => !retainedAssets.has(key));
                // OPFS holds the audio. These reads access file descriptors and Blob sizes only.
                const [fileInfo, artwork] = await Promise.all([
                    Promise.all(fileKeys.map(key => requests(get('files').get(key)))),
                    Promise.all(assetKeys.map(key => requests(get('assets').get(key)))),
                ]);
                discardedFiles = fileInfo.filter(Boolean);
                bytes = discardedFiles.reduce((n,f) => n + f.bytes, 0) + artwork.reduce((n,a) => n + (a?.size || 0), 0);
                if (deleting) {
                    // Reading plays and checking cutoffs in this same write transaction protects a newly played track.
                    for (const id of cancelUnstored ? selected : ids) {
                        for (const name of ['tracks','plays','pending','lyrics']) get(name).delete(id);
                    }
                    for (const key of fileKeys) get('files').delete(key);
                    for (const key of assetKeys) get('assets').delete(key);
                    for (const id of releases) if (!retainedReleases.has(id)) get('meta').delete('release/'+id);
                    if (ids.length) {
                        get('meta').put(Math.max(0,(used || 0) - bytes),'bytes');
                        get('meta').put(crypto.randomUUID(),'revision');
                    }
                }
                await finished;
            } catch (error) {
                try { tx.abort(); } catch { /* already completed or aborted */ }
                await finished.catch(() => {});
                throw error;
            }
            // Commit the index first so an interrupted removal can only leave an unreferenced file.
            if (deleting && discardedFiles.length) {
                try {
                    const dir = await this.directory();
                    for (const file of discardedFiles) {
                        try { await dir.removeEntry(file.name); }
                        catch (error) { if (error.name !== 'NotFoundError') throw error; }
                    }
                } catch (error) {
                    this._reconciled = false;
                    throw error;
                }
            }
            return {ids, count:ids.length, bytes};
        }
        async clear() {
            await navigator.locks.request(this.name, async () => {
                await this.write(stores, get => { for (const store of stores) get(store).clear(); });
                const root = await navigator.storage.getDirectory();
                try { await root.removeEntry(this.name, {recursive:true}); }
                catch (error) { if (error.name !== 'NotFoundError') throw error; }
            });
        }

        async catalog() {
            const revision = await this.read('meta','revision');
            if (this._catalog && this._revision === revision) return this._catalog;
            const rows = await this.all('tracks');
            const tracks = rows.map(row => row.track);
            const artists = new Map(), releases = new Map();
            for (const row of rows) {
                const t = row.track;
                const release = releases.get(t.release_id) || {...row.release, tracks:[]};
                release.tracks.push(t);
                releases.set(release.id, release);
                for (const a of [...t.artists, ...(t.featured_artists || []), ...row.release.artists]) {
                    if (!validId(a?.id)) continue;
                    const artist = artists.get(a.id) || {id:a.id,name:a.name,image_url:null,
                        releases:new Map(), top_tracks:new Map(), featured_tracks:new Map()};
                    if (row.artistImages[a.id]) artist.image_url = row.artistImages[a.id];
                    if (row.release.artists.some(credit => credit.id === a.id)) artist.releases.set(release.id,release);
                    if (t.artists.some(credit => credit.id === a.id)) artist.top_tracks.set(t.id,t);
                    else if (t.featured_artists?.some(credit => credit.id === a.id)) artist.featured_tracks.set(t.id,t);
                    artists.set(a.id,artist);
                }
            }
            for (const release of releases.values()) {
                release.tracks.sort((a,b) => (a.disc_number ?? 1)-(b.disc_number ?? 1) ||
                    (a.track_number ?? Infinity)-(b.track_number ?? Infinity) || a.id-b.id);
            }
            const card = r => ({id:r.id,title:r.title,release_type:r.release_type,year:r.year,
                cover_url:r.cover_url,track_count:r.tracks.length,artists:r.artists.map(a => a.name),uploaders:r.uploaders});
            const artistCards = new Map();
            for (const artist of artists.values()) {
                artist.releases = [...artist.releases.values()].sort((a,b) => (b.year||0)-(a.year||0)).map(card);
                artist.top_tracks = [...artist.top_tracks.values()].sort((a,b) => (b.lastfm_rating||0)-(a.lastfm_rating||0));
                artist.featured_tracks = [...artist.featured_tracks.values()];
                const credited = [...artist.top_tracks,...artist.featured_tracks];
                const recorded = new Set(credited.map(contentKey));
                artist.total_track_count = recorded.size;
                artist.track_content_ids = [...new Set(credited.map(t => t.content_id).filter(Boolean))];
                artist.total_play_count = 0;
                artist.image_url ||= artist.releases.find(r => r.cover_url)?.cover_url || credited.find(t => t.cover_url)?.cover_url || null;
                artistCards.set(artist.id,{id:artist.id,name:artist.name,image_url:artist.image_url,release_count:artist.releases.length,track_count:recorded.size});
            }
            this._revision = revision;
            this._catalog = {tracks, artists, artistCards, releases, releaseCards:[...releases.values()].map(card)};
            return this._catalog;
        }
        async playlist(id, tracks) {
            const byId = new Map(tracks.map(t => [t.id,t])), byContent = new Map(tracks.map(t => [contentKey(t),t]));
            const saved = await this.read('meta','playlists/'+id);
            if (!saved && id !== -1) return null;
            let items;
            if (id === -1) {
                const likes = await this.read('meta','likes');
                const ids = likes?.track_ids;
                const allowed = new Set(ids || []);
                const seen = new Set();
                const ordered = [...(saved?.members || []).map(m => m.id), ...(ids || [])];
                items = [];
                for (const id of ordered) {
                    const t = byId.get(id);
                    if (!t || (ids && !allowed.has(id)) || seen.has(contentKey(t))) continue;
                    seen.add(contentKey(t)); items.push(t);
                }
            } else {
                items = saved.members.flatMap(m => {
                    const track = byId.get(m.id) || byContent.get(m.content_id);
                    return track ? [{...track, playlist_track_id:m.playlist_track_id ?? null, sort_key:m.sort_key}] : [];
                });
            }
            const covers = [...new Set(items.map(t => t.cover_url).filter(Boolean))].slice(0,4);
            const pl = saved ? summarizePlaylist(saved) : {id,kind:'likes',title:'Likes',is_own:true};
            return {...pl, cover_urls:covers, cover_url:covers[0] || null, track_count:items.length, tracks:items};
        }
        async respond(request) {
            const url = new URL(request.url);
            if (url.origin !== location.origin || !url.pathname.startsWith(BASE)) return null;
            const path = url.pathname.slice(BASE.length);
            if (path === 'tracks-by-ids' && request.method === 'POST') {
                const {ids} = await request.json().catch(() => ({}));
                if (!Array.isArray(ids)) return json({error:'Expected track IDs'},400);
                const rows = await Promise.all(ids.slice(0,500).map(trackId).filter(Boolean).map(id => this.read('tracks',id)));
                return json(rows.filter(Boolean).map(row => row.track));
            }
            if (!['GET','HEAD'].includes(request.method)) return null;
            let m;
            if ((m = /^(?:stream\/(\d+)|federation\/cache\/([a-f\d]{64}))$/.exec(path))) {
                const key = m[1] ? (await this.read('tracks',Number(m[1])))?.file : 'b3:'+m[2];
                const file = key && await this.read('files', key);
                if (!file) return missing();
                try {
                    const handle = await (await this.directory()).getFileHandle(file.name);
                    return binary(await handle.getFile(),request,file.mime);
                } catch(error) {
                    if (error.name === 'NotFoundError') return missing();
                    throw error;
                }
            }
            const art = coverKey(url.pathname + url.search);
            if (art) {
                const blob = await this.read('assets', art);
                return blob ? binary(blob,request) : missing();
            }
            if ((m = /^lyrics\/(\d+)$/.exec(path))) {
                const row = await this.read('tracks', Number(m[1]));
                return row ? json(await this.read('lyrics',row.id) || {content_id:row.track.content_id,versions:[]}) : missing();
            }
            if (path === 'federation/tracks/lyrics') {
                const id = trackId('fed:'+url.searchParams.get('content_id'));
                return json(id && await this.read('lyrics',id) || {versions:[]});
            }
            if (/^playlists\/-?\d+\/federation-tracks$/.test(path)) return json([]);
            if (['me','likes','pins','follows','state','tracks/content/like'].includes(path)) {
                const data = await this.read('meta', path);
                if (path === 'me') return data ? json(data) : json({id:this.account,name:'',role:'user',stats:{}});
                if (path === 'state') return json(data || {queue:[],current_track_id:null,position_ms:0,volume:0.7});
                if (path === 'tracks/content/like') return json({content_ids:[]});
                const catalog = await this.catalog();
                if (path === 'likes') {
                    const ids = new Set(catalog.tracks.map(t => t.id));
                    return json({track_ids:(data?.track_ids || []).filter(id => ids.has(id))});
                }
                if (path === 'follows') {
                    const artists = (data?.artist_ids || []).map(id => catalog.artistCards.get(id)).filter(Boolean);
                    return json({artist_ids:artists.map(a => a.id),artists});
                }
                const local = [];
                for (const pin of (data || [])) {
                    if ((pin.kind === 'artist' && catalog.artists.has(pin.id)) ||
                        (pin.kind === 'release' && catalog.releases.has(pin.id)) ||
                        (pin.kind === 'playlist' && (await this.playlist(pin.id,catalog.tracks))?.tracks.length)) local.push(pin);
                }
                return json(local);
            }
            if (!/^(?:artists(?:\/\d+)?|releases\/\d+|search|playlists(?:\/-?\d+)?)$/.test(path)) return null;
            const cat = await this.catalog();
            if (path === 'artists') {
                let list = [...cat.artistCards.values()].sort((a,b) => (!a.release_count)-(!b.release_count) || a.name.localeCompare(b.name));
                if (url.searchParams.get('mine') === 'true') {
                    const owned = await Promise.all(list.map(a => this.read('meta','mine/'+a.id)));
                    list = list.filter((_,i) => owned[i]);
                }
                const page = positiveId(url.searchParams.get('page')) || 1;
                const per_page = Math.min(500, positiveId(url.searchParams.get('limit')) || 80);
                return json({page,per_page,total:list.length,has_more:page*per_page<list.length,items:list.slice((page-1)*per_page,page*per_page)});
            }
            if ((m = /^(artists|releases)\/(\d+)$/.exec(path))) {
                const entry = cat[m[1]].get(Number(m[2]));
                return entry ? json(entry) : missing();
            }
            if (path === 'search') {
                const q = (url.searchParams.get('q') || '').trim().toLocaleLowerCase(), limit = positiveId(url.searchParams.get('limit')) || 10;
                const scores = {artists:{},releases:{},tracks:{}};
                function search(kind, rows, text) {
                    return rows.filter(row => {
                        const [primary,...rest] = text(row).map(value => String(value || '').toLocaleLowerCase());
                        const words = q.split(/\s+/).filter(Boolean), all = [primary,...rest].join(' ');
                        if (!words.length || !words.every(word => all.includes(word))) return false;
                        scores[kind][row.id] = primary === q ? 100 : primary.includes(q) ? 80 : 50;
                        return true;
                    }).sort((a,b) => scores[kind][b.id]-scores[kind][a.id]).slice(0,limit);
                }
                const artists = search('artists',[...cat.artistCards.values()],a => [a.name]);
                const releases = search('releases',cat.releaseCards,r => [r.title,...r.artists]);
                const tracks = search('tracks',cat.tracks,t => [t.title,...t.artists.map(a => a.name),t.release_title,...(t.featured_artists||[]).map(a=>a.name)]);
                return json({scores,artists,releases,tracks,podcasts:[]});
            }
            if ((m = /^playlists\/(-?\d+)$/.exec(path))) {
                const pl = await this.playlist(Number(m[1]),cat.tracks);
                return pl ? json(pl) : missing();
            }
            const ids = (await this.read('meta','playlists') || []).map(p => p.id);
            if (!ids.includes(-1)) ids.unshift(-1);
            const pl = await Promise.all(ids.map(id => this.playlist(id,cat.tracks)));
            return json(pl.filter(p => p && p.track_count).map(({tracks,...card}) => card));
        }
    }
    globalThis.OfflineCatalog = OfflineCatalog;
})();

// Exercise the same OPFS/IndexedDB implementation used by the service worker.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

(async () => {
    const browser = await chromium.launch({headless:true, ...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {})});
    try {
        const page = await browser.newPage();
        await page.route('http://localhost:17455/**', route => route.fulfill(route.request().url().endsWith('.js')
            ? {contentType:'application/javascript',body:fs.readFileSync('src/player/offline-catalog.js','utf8')}
            : {contentType:'text/html',body:'<script src="/offline-catalog.js"></script>'}));
        await page.goto('http://localhost:17455/');
        const results = await page.evaluate(async () => {
            const repo = new OfflineCatalog(1), peerTab = new OfflineCatalog('1'), otherAccount = new OfflineCatalog(2);
            const api = '/api/player/', artist = {id:11,name:'Guest Singer'}, owner = {id:12,name:'Band'};
            const tracks = [21,22,23].map((id,index) => ({id,title:'Song '+id,content_id:'b3:'+(id===23?'2':'1').repeat(64),
                artists:[owner],featured_artists:[artist],release_id:id===22?38:37,release_title:'Album '+index,
                stream_url:api+'stream/'+id,cover_url:api+'cover/13/small',file_size_bytes:6,has_lyrics:true}));
            const requests = [], bytes = new Uint8Array([0,1,2,3,4,5]);
            async function online(url, options = {}) {
                requests.push(url);
                if (url.startsWith(api+'stream/')) return new Response(bytes,{headers:{'Content-Type':'audio/mpeg','Content-Length':'6'}});
                if (url === api+'tracks-by-ids') return Response.json(tracks.filter(t => JSON.parse(options.body).ids.includes(t.id)));
                if (url.startsWith(api+'releases/')) return Response.json({title:'Studio album', artists:[owner],cover_url:api+'cover/13/large'});
                if (url.startsWith(api+'lyrics/')) return Response.json({versions:[{plain_text:'First line'}]});
                if (url.startsWith(api+'cover/')) return new Response(new Uint8Array([1]),{headers:{'Content-Type':'image/jpeg'}});
                throw new Error('Unexpected URL: '+url);
            }
            const read = (path, init) => repo.respond(new Request(location.origin+api+path,init));
            const get = async (path,init) => (await read(path,init)).json();
            await repo.observe(api+'me',{id:1,name:'One'});
            await repo.observe(api+'likes',{track_ids:[21,22,23]});
            await repo.observe(api+'artists?mine=true',{items:[{id:12,image_url:api+'cover/14/medium'}]});
            await repo.observe(api+'playlists',[{id:9,kind:'user',title:'Mix',is_own:true}]);
            await repo.observe(api+'playlists/9',{id:9,kind:'user',title:'Mix',tracks:[tracks[1],{id:999},tracks[0]]});
            await Promise.all([repo.touch(21),peerTab.touch(21),repo.enqueue(tracks[0]),peerTab.enqueue(tracks[0])]);
            const queueBefore = await repo.pending();
            await Promise.all([repo.save(queueBefore[0],{fetch:online,limitBytes:100}),peerTab.save(tracks[1],{fetch:online,limitBytes:100})]);
            const queueAfter = await repo.pending();
            const afterSave = await repo.list();
            const beforeRemove = await repo.stats();
            const accountSeparated = await otherAccount.stats();
            const catalog = (await get('artists')).items;
            const featured = await get('artists/11');
            const release = await get('releases/37');
            const unlikedCopy = (await get('playlists/-1')).tracks;
            const playlist = (await get('playlists/9')).tracks;
            const search = await get('search?q=song+band');
            const lyrics = await get('lyrics/21');
            const cover = await read('cover/14/large');
            const range = await read('stream/21',{headers:{Range:'bytes=2-4'}});
            const rangeBytes = [...new Uint8Array(await range.arrayBuffer())];
            const suffix = await read('stream/21',{headers:{Range:'bytes=-2'}});
            const suffixBytes = [...new Uint8Array(await suffix.arrayBuffer())];
            const invalidRange = await read('stream/21',{headers:{Range:'bytes=6-8'}});
            const head = await read('stream/21',{method:'HEAD'});
            let error;
            try { await repo.save(tracks[2],{fetch:online,limitBytes:8}); } catch(e) {error = e.name;}
            let truncated;
            try {
                await repo.save(tracks[2],{fetch:(path,options) => path.includes('/stream/') ? new Response(new Uint8Array([1])) : online(path,options)});
            } catch {truncated = true;}
            const files = [];
            for await (const name of (await repo.directory()).keys()) files.push(name);
            await repo.remove(21);
            const afterRemove = await repo.stats();
            const remainingBytes = [...new Uint8Array(await (await read('stream/22')).arrayBuffer())];
            await repo.clear();
            const token = 'f'.repeat(64), content = 'b3:'+token, endpoint = api+'federation/cache/'+token;
            const federated = {id:'fed:'+content,content_id:content,title:'Remote song',stream_url:endpoint,
                artists:[{name:'Remote band'}],featured_artists:[{id:artist.id,name:artist.name}],release_title:'Remote album',
                release_key:{normalized_title:'remote album',primary_artists:['remote band'],year:2014},federated_cache:true};
            const rejected = OfflineCatalog.canSave({...federated,stream_url:'https://example.org/untrusted'});
            await repo.touch(federated);
            await repo.enqueue(federated);
            const [queued] = await peerTab.pending();
            await peerTab.save(queued,{fetch:async path => {
                if (path === endpoint) return new Response(bytes,{headers:{'Content-Type':'audio/wav','Content-Length':'6'}});
                if (path.includes('federation/tracks/lyrics?')) return Response.json({versions:[{plain_text:'Remote words'}]});
                throw new Error('Unexpected remote path '+path);
            }});
            const [local] = await repo.list();
            const savedView = await get('releases/'+local.release_id);
            const remoteDetails = {
                rejected,identity:OfflineCatalog.trackId(federated.id) === local.id,
                available:await repo.has(federated),plays:local.play_count, found:(await get('search?q=remote+song')).tracks.length,
                artistCount:(await get('artists/'+local.artists[0].id)).total_track_count,
                featureCount:(await get('artists/'+artist.id)).featured_tracks.length,
                navigable:savedView.tracks[0].stream_url === api+'stream/'+local.id,
                lyrics:(await get('lyrics/'+local.id)).versions[0].plain_text,
                bytes:(await (await read('federation/cache/'+token)).arrayBuffer()).byteLength,
            };
            await repo.remove(federated.id);
            let notFound;
            try { await repo.save({id:999},{fetch:online}); } catch(e) {notFound = e.name;}
            return {queueBefore:queueBefore.length,queueAfter:queueAfter.length,afterSave,beforeRemove,accountSeparated,catalog,
                guests:featured.featured_tracks.map(t=>t.id),albumTracks:release.tracks.map(t=>t.id),unlikedCopy:unlikedCopy.length,
                playlist:playlist.map(t => t.id),results:search.tracks.length,lyrics,cover:cover.status,
                range:[range.status,range.headers.get('Content-Range'),rangeBytes],suffix:suffixBytes,
                invalidRange:invalidRange.status,head:[head.status,head.headers.get('Content-Length'),(await head.arrayBuffer()).byteLength],
                error,truncated,files:files.length,afterRemove,remainingBytes,cleared:await repo.stats(),
                mediaRequests:requests.filter(path => path.includes('/stream/')).length,remoteDetails,notFound};
        });
        assert.equal(results.queueBefore,1);
        assert.equal(results.queueAfter,0);
        assert.equal(results.afterSave.find(t=>t.id===21).play_count,2);
        assert.ok(results.afterSave.every(t => t.downloaded_at>0));
        assert.deepEqual(results.beforeRemove,{tracks:2,bytes:8});
        assert.deepEqual(results.accountSeparated,{tracks:0,bytes:0});
        assert.deepEqual(results.catalog.map(a=>[a.id,a.release_count]),[[12,2],[11,0]]);
        assert.deepEqual(results.guests,[21,22]);
        assert.deepEqual(results.albumTracks,[21]);
        assert.equal(results.unlikedCopy,1);
        assert.deepEqual(results.playlist,[22,21]);
        assert.equal(results.results,2);
        assert.equal(results.lyrics.versions[0].plain_text,'First line');
        assert.equal(results.cover,200);
        assert.deepEqual(results.range,[206,'bytes 2-4/6',[2,3,4]]);
        assert.deepEqual(results.suffix,[4,5]);
        assert.equal(results.invalidRange,416);
        assert.deepEqual(results.head,[200,'6',0]);
        assert.equal(results.error,'QuotaExceededError');
        assert.equal(results.truncated,true);
        assert.equal(results.files,1);
        assert.deepEqual(results.remainingBytes,[0,1,2,3,4,5]);
        assert.deepEqual(results.afterRemove,{tracks:1,bytes:8});
        assert.deepEqual(results.cleared,{tracks:0,bytes:0});
        assert.equal(results.mediaRequests,1);
        assert.deepEqual(results.remoteDetails,{rejected:false,identity:true,available:true,plays:1,found:1,artistCount:1,featureCount:1,
            navigable:true,lyrics:'Remote words',bytes:6});
        assert.equal(results.notFound,'NotFoundError');
        console.log('Offline catalog, federated media, OPFS ranges, indexing, quotas, persistent queue, shared files and account isolation passed.');
    } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode=1; });

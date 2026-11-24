// Render with FURU_PODCAST_RENDER_FIXTURE=1 cargo test renders_podcast_navigation_and_views.
// The server hosts the real member shell and SW. Chromium plays actual PCM audio.
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const assert = require('node:assert/strict');
const {checkCacheManager} = require('./offline-cache-manager.cjs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const player = path.resolve(__dirname, '../src/player');
const html = fs.readFileSync('target/podcast-player-fixture.html','utf8');
const rate = 8000, seconds = 24, wav = Buffer.alloc(44 + rate * 2 * seconds);
wav.write('RIFF'); wav.writeUInt32LE(wav.length-8,4); wav.write('WAVEfmt ',8);
wav.writeUInt32LE(16,16); wav.writeUInt16LE(1,20); wav.writeUInt16LE(1,22);
wav.writeUInt32LE(rate,24); wav.writeUInt32LE(rate*2,28); wav.writeUInt16LE(2,32);
wav.writeUInt16LE(16,34); wav.write('data',36); wav.writeUInt32LE(wav.length-44,40);
const artist = {id:7,name:'Offline band',image_url:null,release_count:1,track_count:2};
const tracks = [1,2].map(id => ({id,content_id:'b3:'+String(id).repeat(64),title:'Recording '+id,
    release_id:9,release_title:'Offline album',release_year:2010,artists:[{id:7,name:artist.name}],featured_artists:[],
    cover_url:null,track_number:id,duration_seconds:seconds,stream_url:'/api/player/stream/'+id,file_size_bytes:wav.length,
    has_lyrics:true,audio_format:'WAV'}));
const release = {id:9,title:'Offline album',year:2010,artists:[artist],tracks,release_type:'Album',uploaders:[]};
const token = 'd'.repeat(64), remoteUrl = '/api/player/federation/cache/'+token;
const prepared = {id:'fed:b3:'+token,content_id:'b3:'+token,title:'Federated recording',release_title:'Remote release',
    artists:[{name:'Remote artist'}],duration_seconds:seconds,stream_url:remoteUrl,federated_cache:true};
const text = {versions:[{id:'v1',label:'Words',plain_text:'Words from the saved recording'}]};
const assets = {'/offline-worker.js':'text/javascript','/offline-catalog.js':'text/javascript',
    '/player.webmanifest':'application/manifest+json','/player-icon.svg':'image/svg+xml'};

(async () => {
    assert.ok(html.includes('data-offline-account="1"'),'render the current member player fixture first');
    let down = false;
    const log = [];
    const server = http.createServer(async (req,res) => {
        const p = new URL(req.url,'http://localhost').pathname;
        log.push({p,method:req.method,range:req.headers.range});
        res.setHeader('Cache-Control','no-store');
        if (p in assets) { res.setHeader('Content-Type',assets[p]); return res.end(fs.readFileSync(path.join(player,p.slice(1)))); }
        if (down) {res.writeHead(503); return res.end();}
        if (p === '/') { res.setHeader('X-Furumusic-Player-Account','1'); res.setHeader('Content-Type','text/html'); return res.end(html); }
        if (p === '/offline-probe') {res.writeHead(204,{'X-Furumusic-Player-Account':'1'}); return res.end();}
        if (p.includes('/stream/') || p === remoteUrl) {
            const range = /^bytes=(\d+)-(\d*)$/.exec(req.headers.range || '');
            const first = range ? Number(range[1]) : 0, last = range && range[2] ? Math.min(Number(range[2]),wav.length-1) : wav.length-1;
            if (first>last) {res.writeHead(416); return res.end();}
            res.setHeader('Content-Type','audio/wav'); res.setHeader('Accept-Ranges','bytes'); res.setHeader('Content-Length',last-first+1);
            if (range) {res.statusCode=206; res.setHeader('Content-Range',`bytes ${first}-${last}/${wav.length}`);}
            return res.end(wav.subarray(first,last+1));
        }
        if (p.endsWith('/events')) {res.writeHead(204); return res.end();}
        const results = {
            '/api/player/me':{id:1,name:'Member',role:'user',stats:{}},
            '/api/player/artists':{items:[artist],page:1,total:1,per_page:80,has_more:false},
            '/api/player/artists/7':{...artist,releases:[release],top_tracks:tracks,featured_tracks:[],total_track_count:2},
            '/api/player/releases/9':release, '/api/player/likes':{track_ids:[1]},
            '/api/player/follows':{artist_ids:[],artists:[]}, '/api/player/playlists':[], '/api/player/pins':[],
            '/api/player/state':{queue:[],repeat_mode:'off',volume:.5},'/api/player/lyrics/1':text,
            '/api/player/federation/tracks/lyrics':text,
            '/api/player/devices/heartbeat':{active_device_id:null,devices:[],jams:[]},
            '/api/player/devices/poll':{active_device_id:null,devices:[],commands:[]},
        };
        res.setHeader('Content-Type','application/json');
        if (p === '/api/player/tracks-by-ids') {
            const body = []; for await (const chunk of req) body.push(chunk);
            const ids = JSON.parse(Buffer.concat(body).toString()).ids;
            return res.end(JSON.stringify(ids.map(id => tracks.find(t => t.id === id)).filter(Boolean)));
        }
        return res.end(JSON.stringify(results[p] || []));
    });
    await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
    const base = 'http://127.0.0.1:'+server.address().port;
    const browser = await chromium.launch({headless:true,...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {})});
    const page = await browser.newPage({viewport:{width:1280,height:860}});
    const errors = [];
    page.on('pageerror',e=>errors.push(e.message));
    try {
        const releaseRow = page.locator('.track-row[data-shared-track-id="1"]');
        const account = page.locator('.sidebar-account-trigger');
        const block = page.locator('#sidebar-account-actions .offline-account');
        await page.goto(base+'/#release/9');
        await releaseRow.waitFor();
        assert.equal(await page.evaluate(() => Alpine.store('offline').enabled),false);
        assert.equal(await page.evaluate(() => navigator.serviceWorker.controller !== null),false);
        await account.click();
        await block.locator('.offline-manage').click();
        await page.locator('.offline-settings [role=switch]').click();
        await page.waitForFunction(() => Alpine.store('offline').enabled && !Alpine.store('offline').busy);
        assert.equal(await page.evaluate(() => Alpine.store('offline').items.length),0);
        await page.locator('.user-settings-head button').click();
        await releaseRow.dblclick();
        await page.waitForFunction(() => Alpine.store('player').isPlaying && Alpine.store('player').currentTime > .1);
        await page.waitForFunction(() => Alpine.store('offline').items.some(t=>t.id===1));
        assert.equal(await page.evaluate(() => Alpine.store('offline').bytes),wav.length);
        await account.click();
        await block.getByRole('switch').click();
        await page.waitForFunction(() => Alpine.store('offline').manual && Alpine.store('offline').active);
        const baseline = log.length;
        down = true;
        await page.goto(base+'/#artist/7');
        await page.reload();
        await page.waitForFunction(() => Alpine.store('offline').active && Alpine.store('library').currentArtist?.id === 7);
        assert.deepEqual(await page.evaluate(() => Alpine.store('library').currentArtist.releases.map(r=>[r.id,r.track_count])),[[9,1]]);
        await page.goto(base+'/#release/9');
        await releaseRow.waitFor();
        assert.equal(await page.locator('.track-row[data-shared-track-id]').count(),1);
        await releaseRow.dblclick();
        await page.waitForFunction(() => Alpine.store('player').isPlaying && Alpine.store('player').currentTime > .3);
        const offlineDetails = await page.evaluate(async () => {
            const response = await fetch('/api/player/stream/1',{headers:{Range:'bytes=0-3'}});
            return [response.status,await response.text(),(await (await fetch('/api/player/lyrics/1')).json()).versions[0].plain_text];
        });
        assert.deepEqual(offlineDetails,[206,'RIFF',text.versions[0].plain_text]);
        const network = log.slice(baseline).filter(r => !['/offline-worker.js','/offline-catalog.js'].includes(r.p));
        assert.deepEqual(network,[], 'manual mode and cold reload performed no origin requests');
        await account.click();
        await block.getByRole('switch').click();
        await page.waitForFunction(() => !Alpine.store('offline').manual && Alpine.store('offline').automatic);
        await page.waitForFunction(() => Alpine.store('offline').automatic);
        down = false;
        await page.waitForFunction(() => !Alpine.store('offline').active, null, {timeout:15000});
        assert.ok(log.slice(baseline).some(r => r.p === '/offline-probe'));
        down = true;
        await page.reload();
        await page.waitForFunction(() => Alpine.store('offline').automatic && Alpine.store('library').currentRelease?.id === 9);
        assert.equal(await page.locator('.track-row[data-shared-track-id]').count(),1);
        down = false;
        await page.waitForFunction(() => !Alpine.store('offline').active,null,{timeout:15000});
        await page.evaluate(track => Alpine.store('player').play(track),prepared);
        await page.waitForFunction(() => Alpine.store('offline').items.some(t=>t.offline_remote));
        await page.evaluate(() => Alpine.store('offline').setManual(true));
        await page.waitForFunction(() => {
            const p = Alpine.store('player');
            return p.currentTrack?.offline_remote && p.isPlaying && p.currentTime > .1;
        });
        const remoteArtist = await page.evaluate(() => Alpine.store('player').currentTrack.artists[0].id);
        await page.goto(base+'/#artist/'+remoteArtist);
        await page.waitForFunction(() => Alpine.store('library').currentArtist?.name === 'Remote artist');
        assert.deepEqual(await page.evaluate(() => Alpine.store('library').currentArtist.releases.map(r=>r.title)),['Remote release']);
        await page.evaluate(() => Alpine.store('player').pause());
        for (const width of [1280,320]) await checkCacheManager(page,log,width);
        assert.deepEqual(errors,[]);
        console.log('Real offline playback, navigation, cold reload, recovery and desktop/mobile cache management passed.');
    } catch (error) {
        const state = await page.evaluate(() => {
            const offline = window.Alpine?.store('offline'), p = window.Alpine?.store('player');
            return {url:location.hash, enabled:offline?.enabled,active:offline?.active,error:offline?.error,
                busy:offline?.busy,saved:offline?.items?.length,playing:p?.isPlaying,track:p?.currentTrack?.id};
        }).catch(()=>({}));
        console.error({state,errors,recent:log.slice(-7)});
        throw error;
    } finally { await browser.close(); server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); }
})().catch(e=>{console.error(e); process.exitCode=1;});

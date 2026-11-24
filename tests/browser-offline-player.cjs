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
const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=', 'base64');
wav.write('RIFF'); wav.writeUInt32LE(wav.length-8,4); wav.write('WAVEfmt ',8);
wav.writeUInt32LE(16,16); wav.writeUInt16LE(1,20); wav.writeUInt16LE(1,22);
wav.writeUInt32LE(rate,24); wav.writeUInt32LE(rate*2,28); wav.writeUInt16LE(2,32);
wav.writeUInt16LE(16,34); wav.write('data',36); wav.writeUInt32LE(wav.length-44,40);
const artist = {id:7,name:'Offline band',image_url:null,release_count:1,track_count:3};
const tracks = [1,2,3].map(id => ({id,content_id:'b3:'+String(id).repeat(64),title:'Recording '+id,
    release_id:9,release_title:'Offline album',release_year:2010,artists:[{id:7,name:artist.name}],featured_artists:[],
    cover_url:'/api/player/cover/'+id+'/small',track_number:id,duration_seconds:seconds,stream_url:'/api/player/stream/'+id,file_size_bytes:wav.length,
    has_lyrics:true,audio_format:'WAV'}));
const release = {id:9,title:'Offline album',year:2010,cover_url:tracks[0].cover_url,artists:[artist],tracks,release_type:'Album',uploaders:[]};
const token = 'd'.repeat(64), remoteUrl = '/api/player/federation/cache/'+token;
const prepared = {id:'fed:b3:'+token,content_id:'b3:'+token,title:'Federated recording',release_title:'Remote release',
    artists:[{name:'Remote artist'}],duration_seconds:seconds,stream_url:remoteUrl,federated_cache:true};
const text = {versions:[{id:'v1',label:'Words',plain_text:'Words from the saved recording'}]};
const assets = {'/offline-worker.js':'text/javascript','/offline-catalog.js':'text/javascript',
    '/player.webmanifest':'application/manifest+json','/player-icon.svg':'image/svg+xml'};

async function waitForCachedAudio(page, trackId, after = .25) {
    await page.waitForFunction(([id, seconds]) => {
        const audio = window.playingAudio;
        return Alpine.store('player').currentTrack?.id === id && audio?.currentSrc.startsWith('blob:') &&
            !audio.paused && !audio.seeking && audio.currentTime > seconds;
    }, [trackId,after], {timeout: 8000});
}

async function waitForCachedCovers(page, selectors) {
    await page.waitForFunction(selectors => selectors.every(selector => {
        const img = document.querySelector(selector);
        return img?.naturalWidth === 1 && /^(blob:|data:)/.test(img.currentSrc);
    }), selectors);
}

(async () => {
    assert.ok(html.includes('data-offline-account="1"'),'render the current member player fixture first');
    let down = false, remoteOwner = null;
    const log = [];
    const server = http.createServer(async (req,res) => {
        const p = new URL(req.url,'http://localhost').pathname;
        log.push({p,method:req.method,range:req.headers.range});
        res.setHeader('Cache-Control','no-store');
        if (p in assets) { res.setHeader('Content-Type',assets[p]); return res.end(fs.readFileSync(path.join(player,p.slice(1)))); }
        if (down) {res.writeHead(503); return res.end();}
        if (p === '/') { res.setHeader('X-Furumusic-Player-Account','1'); res.setHeader('Content-Type','text/html'); return res.end(html); }
        if (p === '/offline-probe') {res.writeHead(204,{'X-Furumusic-Player-Account':'1'}); return res.end();}
        if (/^\/api\/player\/cover\/\d+\/(small|medium|large)$/.test(p)) {
            res.setHeader('Content-Type','image/png');
            return res.end(png);
        }
        if (p.includes('/stream/') || p === remoteUrl) {
            const range = /^bytes=(\d+)-(\d*)$/.exec(req.headers.range || '');
            const first = range ? Number(range[1]) : 0, last = range && range[2] ? Math.min(Number(range[2]),wav.length-1) : wav.length-1;
            if (first>last) {res.writeHead(416); return res.end();}
            res.setHeader('Content-Type','audio/wav'); res.setHeader('Accept-Ranges','bytes'); res.setHeader('Content-Length',last-first+1);
            if (range) {res.statusCode=206; res.setHeader('Content-Range',`bytes ${first}-${last}/${wav.length}`);}
            return res.end(wav.subarray(first,last+1));
        }
        if (p === '/api/player/federation/artists/events') {
            res.writeHead(200, {'Content-Type':'text/event-stream'});
            res.write(': connected\n\n');
            return;
        }
        if (p.endsWith('/events')) {res.writeHead(204); return res.end();}
        const results = {
            '/api/player/me':{id:1,name:'Member',role:'user',stats:{}},
            '/api/player/artists':{items:[artist],page:1,total:1,per_page:80,has_more:false},
            '/api/player/artists/7':{...artist,releases:[release],top_tracks:tracks,featured_tracks:[],total_track_count:tracks.length},
            '/api/player/releases/9':release, '/api/player/likes':{track_ids:[1]},
            '/api/player/playlists/-1':{id:-1,title:'Likes',kind:'likes',is_own:true,cover_urls:[tracks[0].cover_url],tracks:[tracks[0]]},
            '/api/player/follows':{artist_ids:[],artists:[]}, '/api/player/playlists':[], '/api/player/pins':[],
            '/api/player/state':{queue:[],repeat_mode:'off',volume:.5},'/api/player/lyrics/1':text,
            '/api/player/federation/tracks/lyrics':text,
            '/api/player/devices/heartbeat':{active_device_id:remoteOwner,devices:[],jams:[]},
            '/api/player/devices/poll':{active_device_id:remoteOwner,devices:[],commands:[],
                playback_state:remoteOwner ? {track:tracks[0],tracks:[tracks[0]],index:0,position_seconds:6,paused:false} : null},
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
    const context = await browser.newContext({viewport:{width:1280,height:860}});
    const page = await context.newPage();
    await context.addInitScript(() => {
        const play = HTMLMediaElement.prototype.play;
        window.audioPlayCalls = 0;
        HTMLMediaElement.prototype.play = function(...args) {
            if (this instanceof HTMLAudioElement) {
                window.audioPlayCalls++;
                window.playingAudio = this;
            }
            return play.apply(this,args);
        };
    });
    const errors = [];
    page.on('pageerror',e=>errors.push(e.message));
    const httpMedia = [], httpCovers = [];
    page.on('request', req => {
        if (req.resourceType() === 'media' && /^https?:/.test(req.url())) httpMedia.push(req.url());
        if (req.url().startsWith(base+'/api/player/cover/')) httpCovers.push(req.url());
    });
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
        assert.equal(await page.evaluate(() => Alpine.store('offline').bytes),wav.length+png.length);
        await page.evaluate(track => Alpine.store('offline').saveTrack(track),tracks[1]);
        await page.waitForFunction(() => Alpine.store('offline').items.length === 2);

        // Disconnect during the first play, which started on HTTP before the cache was populated.
        await page.evaluate(() => Alpine.store('player').seek(12));
        await page.waitForFunction(() => window.playingAudio.currentTime > 12);
        const position = await page.evaluate(() => window.playingAudio.currentTime);
        const beforeDisconnect = httpMedia.length;
        const devtools = await context.newCDPSession(page);
        await devtools.send('Network.enable');
        await devtools.send('Network.setCacheDisabled', {cacheDisabled:true});
        await context.setOffline(true);
        await page.waitForFunction(() => Alpine.store('offline').automatic);
        await waitForCachedAudio(page, 1, position);
        await waitForCachedCovers(page, ['.release-cover img', '.player-cover img']);
        const coversAtDisconnect = httpCovers.length;
        await page.evaluate(() => Alpine.store('player').next());
        await waitForCachedAudio(page, 2);
        await page.evaluate(pos => Alpine.store('player').seek(pos),seconds / 2);
        await waitForCachedAudio(page, 2, seconds / 2);
        await page.goto(base+'/#playlist/-1');
        await page.waitForFunction(() => Alpine.store('library').currentPlaylist?.id === -1);
        await waitForCachedCovers(page, ['.playlist-heading-cover img', '.player-cover img']);
        await page.locator('.playlist-track-row[data-playlist-index="0"]').dblclick();
        await waitForCachedAudio(page, 1);
        assert.deepEqual(httpCovers.slice(coversAtDisconnect), [], 'opening likes and switching tracks do not request HTTP artwork');
        assert.deepEqual(httpMedia.slice(beforeDisconnect), [], 'playback and seeking use local files without HTTP media requests');
        await context.setOffline(false);
        await devtools.send('Network.setCacheDisabled', {cacheDisabled:false});
        await devtools.detach();
        await page.waitForFunction(() => !Alpine.store('offline').active);
        await page.goto(base+'/#release/9');
        await releaseRow.dblclick();
        await waitForCachedAudio(page, 1);
        // A second, forgotten tab sees the playing track through device sync.
        remoteOwner = await page.evaluate(() => Alpine.store('devices').id);
        const controller = await context.newPage();
        controller.on('pageerror',e=>errors.push(e.message));
        await controller.goto(base+'/#artist/7');
        await controller.waitForFunction(() => !Alpine.store('devices').isActive() &&
            Alpine.store('player').currentTrack?.id === 1 && Alpine.store('player').isPlaying);
        await account.click();
        await block.getByRole('switch').click();
        await page.waitForFunction(() => Alpine.store('offline').manual && Alpine.store('offline').active);
        await page.waitForFunction(() => {
            const player = Alpine.store('player');
            return player._offlineSession && player.isPlaying && player.currentTime > .3;
        });
        await controller.waitForFunction(() => Alpine.store('offline').active && Alpine.store('player')._offlineSession &&
            !Alpine.store('player').isPlaying);
        assert.equal(await controller.evaluate(() => window.audioPlayCalls),0,'only the original playback tab may continue');
        await controller.close();
        remoteOwner = null;
        await page.locator('.device-toggle-btn.active:visible').waitFor();
        const baseline = log.length;
        down = true;
        await page.goto(base+'/#artist/7');
        await page.reload();
        await page.waitForFunction(() => Alpine.store('offline').active && Alpine.store('library').currentArtist?.id === 7);
        assert.deepEqual(await page.evaluate(() => Alpine.store('library').currentArtist.releases.map(r=>[r.id,r.track_count])),[[9,2]]);
        await page.goto(base+'/#release/9');
        await releaseRow.waitFor();
        assert.equal(await page.locator('.track-row[data-shared-track-id]').count(),2);
        await releaseRow.dblclick();
        await waitForCachedAudio(page, 1);
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
        assert.equal(await page.locator('.track-row[data-shared-track-id]').count(),2);
        down = false;
        await page.waitForFunction(() => !Alpine.store('offline').active,null,{timeout:15000});
        // Lose the browser connection while viewing a live artist and controlling another device.
        remoteOwner = 'other-browser';
        await page.goto(base+'/#artist/7');
        await page.waitForFunction(() => !Alpine.store('devices').isActive() && Alpine.store('player').isPlaying &&
            Alpine.store('player').currentTrack?.id === 1 &&
            Alpine.store('library')._artistFederationSource?.readyState === EventSource.OPEN);
        const controllerPlayCalls = await page.evaluate(() => window.audioPlayCalls);
        await page.context().setOffline(true);
        await page.waitForFunction(() => Alpine.store('offline').automatic &&
            !Alpine.store('library')._artistFederationSource && Alpine.store('library').currentArtist?.top_tracks?.length === 2);
        await page.locator('.device-toggle-btn.active:visible').waitFor();
        await page.waitForFunction(() => Alpine.store('player')._offlineSession && !Alpine.store('player').isPlaying);
        assert.equal(await page.evaluate(() => window.audioPlayCalls),controllerPlayCalls,'a former controller must not start audio on disconnection');
        const disconnected = log.length;
        const savedRow = page.locator('.artist-appearance-row:visible').filter({hasText:'Recording 1'}).first();
        await savedRow.press('Enter');
        await waitForCachedAudio(page, 1);
        assert.equal(await page.evaluate(() => Alpine.store('library').federationIsLiked({key:{content_id:'unknown'}})),false);
        const artistEvents = log.filter(r => r.p === '/api/player/federation/artists/events').length;
        await page.reload();
        await page.waitForFunction(() => Alpine.store('offline').automatic && Alpine.store('library').currentArtist?.id === 7);
        assert.equal(await page.evaluate(() => window.audioPlayCalls),0,'reloading offline must not autoplay');
        assert.deepEqual(log.slice(disconnected).filter(r => !['/offline-worker.js','/offline-catalog.js'].includes(r.p)),[]);
        remoteOwner = null;
        await page.context().setOffline(false);
        await page.waitForFunction(() => !Alpine.store('offline').active &&
            Alpine.store('library')._artistFederationSource?.readyState === EventSource.OPEN,null,{timeout:15000});
        assert.equal(log.filter(r => r.p === '/api/player/federation/artists/events').length,artistEvents+1);
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

// Render with FURU_PODCAST_RENDER_FIXTURE=1 cargo test renders_podcast_navigation_and_views.
const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = readFileSync('target/podcast-player-fixture.html', 'utf8');
const alpine = readFileSync('target/podcast-alpine.min.js', 'utf8');
const song = (id, content_id, sort_key) => ({
    id, content_id, sort_key, title:'Taking Off', duration_seconds:219,
    artists:[{id:1,name:'ONE OK ROCK'}], stream_url:'/api/player/stream/'+id,
});
const original = song(5672, 'b3:'+'a'.repeat(64), 30);
const alias = {...original,id:5686};
const other = song(5797, 'b3:'+'b'.repeat(64), 20);
const remote = song(null, 'b3:'+'c'.repeat(64), 10);
const reference = track => ({
    position:track.sort_key, content_id:track.content_id,
    federation:{content_id:track.content_id, title:track.title, artist_names:['ONE OK ROCK'],
        owner:'peer', item_id:track.content_id, duration_seconds:219},
});

async function check(browser) {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', err => errors.push(err.message));
    let liked = true, delay = false, releaseResponse;
    const writes = [];
    await page.route('**/*', async route => {
        const {hostname, pathname} = new URL(route.request().url());
        if (hostname === 'likes.test' && pathname === '/') return route.fulfill({contentType:'text/html',body:html});
        if (hostname === 'cdn.jsdelivr.net' && pathname.includes('alpine')) return route.fulfill({contentType:'application/javascript',body:alpine});
        const markers = liked ? [original,alias,other] : [other];
        const locals = liked && writes.includes(alias.id) ? [alias,other] : markers;
        if (pathname === '/api/player/playlists/-1/federation-tracks') {
            const rows = (liked ? [original,remote] : [remote]).map(reference);
            if (delay) {
                delay = false;
                await new Promise(resolve => {releaseResponse = resolve;});
            }
            return route.fulfill({json:rows});
        }
        if (pathname.startsWith('/api/player/likes/toggle/')) {
            writes.push(Number(pathname.split('/').at(-1)));
            liked = !liked;
            return route.fulfill({json:{liked,content_id:original.content_id,affected_track_ids:[original.id,alias.id]}});
        }
        const json = {
            '/api/player/playlists/-1':{id:-1,kind:'likes',title:'Likes',tracks:locals},
            '/api/player/playlists':[{id:-1,kind:'likes',track_count:liked ? 3 : 2}],
            '/api/player/likes':{track_ids:markers.map(t => t.id)},
            '/api/player/releases/7':{id:7,title:'Ambitions',artists:original.artists,uploaders:[],tracks:[original,alias,other]},
            '/api/player/tracks/content/like':(liked ? [original,other,remote] : [other,remote]).map(t => t.content_id),
            '/api/player/artists':{items:[],total:0,has_more:false},
            '/api/player/me':{id:1,name:'Listener'},
        };
        return route.fulfill({json:json[pathname] || []});
    });
    const rows = page.locator('.playlist-track-row');
    const waitForIds = ids => page.waitForFunction(expected => {
        const visible = Alpine.store('library').currentPlaylist?.tracks.map(t => t.id) || [];
        return JSON.stringify(visible) === JSON.stringify(expected);
    }, ids);
    await page.goto('http://likes.test/#playlist/-1');
    const remoteId = 'fed:'+remote.content_id;
    await waitForIds([original.id,other.id,remoteId]);
    assert.equal(await rows.count(), 3);
    await page.evaluate(() => {
        Alpine.store('queue').playRelease = (tracks, index) => { window.chosen = tracks[index]; };
    });
    await rows.nth(1).dblclick();
    assert.equal(await page.evaluate(() => window.chosen.id), other.id);
    await page.goto('http://likes.test/#artists');
    await page.waitForFunction(() => Alpine.store('library').view === 'artists');
    delay = true;
    const pending = page.waitForRequest(req => req.url().endsWith('/-1/federation-tracks'));
    await page.goto('http://likes.test/#playlist/-1');
    await pending;
    await rows.first().locator('.like-btn').click();
    await waitForIds([other.id,remoteId]);
    assert.deepEqual(writes,[original.id]);
    const oldResponse = page.waitForResponse(res => res.url().endsWith('/-1/federation-tracks'));
    releaseResponse();
    await (await oldResponse).finished();
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    await waitForIds([other.id,remoteId]);
    assert.deepEqual(await page.evaluate(([id,alias,other,content]) => {
        const likes = Alpine.store('likes');
        return [likes.has(id),likes.has(alias),likes.has(other),Alpine.store('library').federationLiked.includes(content)];
    },[original.id,alias.id,other.id,original.content_id]), [false,false,true,false]);
    await page.reload();
    await waitForIds([other.id,remoteId]);
    await page.goto('http://likes.test/#release/7');
    const releaseHeart = id => page.locator('[data-shared-track-id="'+id+'"] .like-btn');
    await releaseHeart(alias.id).click();
    await page.waitForFunction(id => Alpine.store('likes').has(id), original.id);
    for (const track of [original,alias,other]) assert.ok(await releaseHeart(track.id).evaluate(el => el.classList.contains('liked')));
    await page.goto('http://likes.test/#playlist/-1');
    await waitForIds([alias.id,other.id,remoteId]);
    assert.deepEqual(writes,[original.id,alias.id]);
    assert.deepEqual(errors, []);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({
        headless:true, ...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {}),
    });
    try {
        await check(browser);
        console.log('Liked song identities, source merging, playback and removal passed.');
    } finally { await browser.close(); }
})().catch(err => { console.error(err); process.exitCode = 1; });

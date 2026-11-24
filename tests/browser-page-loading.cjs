// Render the player with FURU_PODCAST_RENDER_FIXTURE=1 before running.
const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const artist = {id:42, name:'Echo', releases:[], top_tracks:[], featured_tracks:[], total_track_count:0, total_play_count:0};
const playlist = {id:7, title:'First light', kind:'user', tracks:[], is_own:true};
const pins = [
    {kind:'artist', title:artist.name, href:'#artist/42'},
    {kind:'playlist', title:playlist.title, href:'#playlist/7'},
];

async function holdResponse(page, pathname) {
    let release;
    const gate = new Promise(resolve => { release = resolve; });
    await page.route('**' + pathname, async route => route.fulfill({json:await gate}), {times:1});
    return async data => {
        const response = page.waitForResponse(r => new URL(r.url()).pathname === pathname);
        release(data);
        await (await response).finished();
        await page.evaluate(() => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done))));
    };
}

(async () => {
    const browser = await chromium.launch({headless:true, ...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {})});
    try {
        const page = await browser.newPage({viewport:{width:1200,height:800}});
        page.setDefaultTimeout(5000);
        const errors = [];
        page.on('pageerror', error => errors.push(error.message));
        const html = readFileSync('target/podcast-player-fixture.html','utf8');
        await page.route('**/*', route => {
            const {pathname} = new URL(route.request().url());
            if (pathname === '/') return route.fulfill({contentType:'text/html', body:html});
            if (route.request().resourceType() === 'eventsource') return route.fulfill({status:204});
            const responses = {
                '/api/player/artists':{items:[], total:0, has_more:false}, '/api/player/artists/42':artist,
                '/api/player/pins':pins, '/api/player/playlists':[playlist], '/api/player/me':{id:1, name:'Listener'},
            };
            return route.fulfill({json:responses[pathname] || []});
        });
        const finishArtist = await holdResponse(page, '/api/player/artists/42');
        const finishPlaylist = await holdResponse(page, '/api/player/playlists/7');
        await page.goto('http://loading.test/');
        const shortcuts = page.locator('.sidebar-left .music-shortcuts');
        const loading = page.locator('#center-scroll .page-skeleton:visible');
        await shortcuts.getByRole('link', {name:artist.name}).click();
        await loading.waitFor();
        assert.equal(await loading.getAttribute('data-skeleton'), 'artist');
        await shortcuts.getByRole('link', {name:playlist.title}).click();
        await page.locator('.page-skeleton[data-skeleton="playlist"]').waitFor();
        await finishArtist(artist);
        assert.equal(await loading.getAttribute('data-skeleton'), 'playlist', 'a late artist response must leave the playlist loading');
        await finishPlaylist(playlist);
        await page.getByRole('heading', {name:playlist.title}).waitFor();
        await loading.waitFor({state:'hidden'});
        await shortcuts.getByRole('link', {name:artist.name}).click();
        await page.locator('.artist-header').getByText(artist.name, {exact:true}).waitFor();
        await loading.waitFor({state:'hidden'});
        assert.deepEqual(errors, []);
        console.log('Quick access skeletons, stale responses and loaded pages passed.');
    } finally {
        await browser.close();
    }
})().catch(error => { console.error(error); process.exitCode = 1; });

// Render the player with FURU_PODCAST_RENDER_FIXTURE=1 cargo test renders_podcast_navigation_and_views.
const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = readFileSync('target/podcast-player-fixture.html', 'utf8');
const files = Array.from({length: 40}, (_, index) => {
    const name = `Track ${index + 1}.flac`;
    return {index, name, components: [name], length: 4000000, selected: true};
});
const preview = {id: 'tour', name: 'Tour recordings', total_size: 160000000, files};
const job = {id: preview.id, name: preview.name, status: 'preview'};

async function checkManager(browser, width, height) {
    const page = await browser.newPage({viewport: {width, height}});
    page.setDefaultTimeout(5000);
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    let session, started;
    await page.route('**/*', route => {
        const {pathname} = new URL(route.request().url());
        if (pathname === '/') return route.fulfill({contentType: 'text/html', body: html});
        if (pathname === '/api/player/torrents/preview') {
            session = job;
            return route.fulfill({json: {preview, job}});
        }
        if (pathname === '/api/player/torrents/tour/start') {
            started = route.request().postDataJSON();
            session = {...job, status: 'downloading', active: true};
            return route.fulfill({json: session});
        }
        const responses = {
            '/api/player/artists': {items: [], total: 0, has_more: false},
            '/api/player/me': {id: 1, name: 'Listener'},
            '/api/player/playlists': [{id: 7, kind: 'user', is_own: true, title: 'On tour'}],
            '/api/player/uploads/local/history': [{id: 'encore', filename: 'Encore.wav', status: 'complete', created_at: '2026-11-23T12:00:00Z'}],
            '/api/player/uploads/tracks': {queued_total: 1, queued: [{id: 9, filename: 'Soundcheck.flac', status: 'queued'}]},
            '/api/player/torrents': session ? [session] : [],
            '/api/player/youtube/preview': {title: 'Video playlist', items: [{source_id: 'clip', title: 'Live in concert', selected_by_default: true}]},
        };
        return route.fulfill({json: responses[pathname] || []});
    });
    await page.goto('https://downloads.test/');
    await page.locator('.torrent-import-btn').click();
    const dialog = page.locator('.downloads-dialog');
    const tab = source => dialog.locator('#downloads-tab-' + source);
    const overview = dialog.locator('#downloads-overview');
    await overview.getByText('Encore.wav', {exact:true}).waitFor();
    await overview.getByRole('combobox').selectOption('active');
    assert.equal(await overview.getByText('Encore.wav', {exact:true}).count(), 0);
    await overview.getByRole('combobox').selectOption('history');
    await overview.getByText('Encore.wav', {exact:true}).waitFor();
    if (process.env.DOWNLOADS_SCREENSHOTS) {
        await page.screenshot({path:`target/downloads-overview-${width}x${height}.png`, animations:'disabled'});
    }
    await overview.getByRole('button', {name: /Encore\.wav/}).click();
    await dialog.locator('#download-file-job-encore').waitFor();
    await page.waitForFunction(() => document.activeElement?.id === 'download-file-job-encore');
    await tab('overview').click();
    await dialog.locator('button[aria-controls="download-add-list"]').click();
    await dialog.locator('#download-add-list').getByRole('button', {name: 'YouTube'}).click();
    const youtube = dialog.locator('#youtube-download-url');
    await youtube.fill('https://www.youtube.com/watch?v=demo');
    await youtube.press('Enter');
    await dialog.getByText('Live in concert', {exact: true}).waitFor();
    await tab('files').click();
    await dialog.locator('#direct-audio-file-input').setInputFiles({
        name: 'live.wav', mimeType: 'audio/wav', buffer: Buffer.from('audio'),
    });
    await dialog.locator('#local-target-playlist').selectOption('7');
    assert.ok(await dialog.getByRole('button', {name:'Upload selected files'}).isEnabled());
    await tab('uploads').click();
    await dialog.getByText('Soundcheck.flac', {exact:true}).waitFor();
    await tab('torrents').click();
    await dialog.locator('.torrent-add').click();
    const magnet = dialog.locator('#torrent-magnet-input');
    await magnet.fill('magnet:?xt=urn:btih:0123456789012345678901234567890123456789');
    await dialog.locator('.torrent-preview-submit').click();
    const tree = dialog.locator('.torrent-file-tree');
    await tree.getByText(files[0].name, {exact: true}).waitFor();
    const metrics = await tree.evaluate(el => {
        const box = el.getBoundingClientRect();
        const row = el.querySelector('.torrent-tree-row').getBoundingClientRect();
        return {y: box.y, bottom: box.bottom, view: el.clientHeight, row: row.height, full: el.scrollHeight};
    });
    assert.ok(metrics.view >= 5 * metrics.row - 2, 'at least five files should fit in the preview ' + JSON.stringify(metrics));
    assert.ok(metrics.y >= 0 && metrics.bottom <= height + 1 && metrics.full > metrics.view, 'tree should scroll inside the screen');
    assert.ok(await dialog.evaluate(el => el.scrollWidth <= el.clientWidth + 1), 'no horizontal dialog overflow');
    await tree.getByRole('checkbox').last().uncheck();
    await dialog.locator('#torrent-target-playlist').selectOption('7');
    if (process.env.DOWNLOADS_SCREENSHOTS) {
        await tree.evaluate(el => {el.scrollTop = 0;});
        await page.screenshot({path: `target/downloads-${width}x${height}.png`, animations:'disabled'});
    }
    await Promise.all([
        page.waitForResponse(res => new URL(res.url()).pathname === '/api/player/torrents/tour/start'),
        dialog.locator('.torrent-start').click(),
    ]);
    assert.equal(started?.target_playlist_id, 7);
    assert.deepEqual(started?.selected_files, files.slice(0, -1).map(file => file.index));
    await page.keyboard.press('Escape');
    await dialog.waitFor({state: 'hidden'});
    assert.deepEqual(errors, []);
    console.log(`${width}×${height}: torrent preview ${metrics.view}px; ${Math.floor(metrics.view / metrics.row)} complete rows`);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({headless:true, ...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {})});
    try {
        for (const [width, height] of [[1280, 800], [390, 844], [320, 568]]) await checkManager(browser, width, height);
    } finally {
        await browser.close();
    }
})().catch(error => {console.error(error); process.exitCode = 1;});

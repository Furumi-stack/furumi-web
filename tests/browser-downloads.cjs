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
const date = '2026-11-23T12:00:00Z';
const historyPath = '/api/player/downloads/history';
function transfer(source, id, title, item_count, status = 'complete') {
    return {source, id, title, item_count, status, updated_at: date, created_at: date, progress_percent: null, error: null};
}
async function modalStyle(surface) {
    return surface.evaluate(el => {
        const close = getComputedStyle(el.querySelector('.player-dialog-close'));
        const head = getComputedStyle(el.querySelector('.player-dialog-head'));
        const panel = getComputedStyle(el);
        return {
            panel: [panel.backgroundColor, panel.borderTopColor, panel.borderRadius],
            close: [close.width, close.height, close.borderRadius, close.backgroundColor],
            heading: [head.borderBottomColor, head.borderBottomWidth, head.alignItems],
        };
    });
}

async function checkManager(browser, width, height) {
    const page = await browser.newPage({viewport: {width, height}});
    page.setDefaultTimeout(5000);
    if (width === 320) await page.emulateMedia({reducedMotion: 'reduce'});
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    let session, started, videoStarted, hidden = [];
    let history = [
        transfer('files', 'batch-one', 'Festival recordings', 120),
        transfer('youtube', 'show', 'Late show', 5, 'queued'),
    ];
    const stats = {tracks: 120, releases: 28};
    const uploaded = [];
    const requested = [];
    await page.route('**/*', route => {
        const req = route.request();
        const {pathname, searchParams} = new URL(req.url());
        requested.push(pathname);
        if (pathname === '/') return route.fulfill({contentType: 'text/html', body: html});
        if (pathname === historyPath) {
            if (searchParams.has('id')) {
                const item = history.find(row => row.id === searchParams.get('id') && row.source === searchParams.get('source'));
                if (item?.id === 'show') item.status = 'complete';
                return route.fulfill({json: {items:item ? [item] : [], has_more:false, stats}});
            }
            const offset = Number(searchParams.get('offset') || 0), limit = Number(searchParams.get('limit') || 30);
            return route.fulfill({json: {items: history.slice(offset, offset + limit), has_more: offset + limit < history.length, stats}});
        }
        if (pathname === historyPath + '/hide') {
            assert.equal(req.method(), 'POST');
            hidden = req.postDataJSON().items;
            history = history.filter(row => !hidden.some(item => item.id === row.id && item.source === row.source));
            return route.fulfill({json: {ok: true}});
        }
        if (pathname === '/api/player/uploads/local') {
            const headers = req.headers();
            if (headers['x-furumusic-upload-complete'] === 'true') {
                history.unshift(transfer('files', headers['x-furumusic-upload-batch-id'], null, uploaded.length, 'queued'));
            } else {
                uploaded.push(headers);
            }
            return route.fulfill({json:{ok:true}});
        }
        if (pathname === '/api/player/youtube/start') {
            videoStarted = req.postDataJSON();
            const video = transfer('youtube', 'concert', 'Live in concert', 1, 'queued');
            history.unshift(video);
            return route.fulfill({json: {...video, items: []}});
        }
        if (pathname === '/api/player/torrents/preview') {
            session = job;
            history.unshift(transfer('torrents', job.id, job.name, files.length, 'preview'));
            return route.fulfill({json: {preview, job}});
        }
        if (pathname === '/api/player/torrents/tour/start') {
            started = req.postDataJSON();
            session = {...job, status: 'downloading', active: true};
            history.find(row => row.id === job.id).status = 'downloading';
            return route.fulfill({json: session});
        }
        const responses = {
            '/api/player/artists': {items: [], total: 0, has_more: false},
            '/api/player/me': {id: 1, name: 'Listener'},
            '/api/player/playlists': [{id: 7, kind: 'user', is_own: true, title: 'On tour'}],
            '/api/player/torrents/session/tour': {preview, job: session},
            '/api/player/torrents/tour/status': session,
            '/api/player/youtube/preview': {source_url:'https://www.youtube.com/watch?v=demo', title: 'Live in concert', items: [{source_id: 'clip', title: 'Opening song', selected_by_default: true}]},
        };
        return route.fulfill({json: responses[pathname] || []});
    });
    await page.goto('https://downloads.test/');
    await page.locator('.torrent-import-btn').click();
    const dialog = page.locator('.downloads-dialog');
    await dialog.locator('.player-dialog-close').waitFor();
    const downloadStyle = await modalStyle(dialog.locator('.player-dialog-surface'));
    const tab = source => dialog.locator('#downloads-tab-' + source);
    const statsView = dialog.locator('#downloads-stats');
    const historyView = dialog.locator('#downloads-history');
    const screenshot = async name => {
        if (process.env.DOWNLOADS_SCREENSHOTS) {
            await page.screenshot({path: `target/downloads-${name}-${width}x${height}.png`, animations:'disabled'});
        }
    };
    await dialog.getByRole('button', {name:'Choose files', exact:true}).waitFor();
    await page.waitForFunction(() => document.querySelector('#downloads-stats')?.textContent.includes('120'));
    assert.match(await statsView.innerText(), /120\s+tracks\s*[·/]?\s*28\s+releases/i);
    assert.equal(await tab('files').getAttribute('aria-selected'), 'true');
    assert.equal(await historyView.isVisible(), false);
    assert.equal(await dialog.getByText('My uploaded tracks', {exact:true}).count(), 0);
    assert.equal(await dialog.getByRole('button', {name:/approve/i}).count(), 0);
    assert.deepEqual(requested.filter(path => ['/api/player/uploads/tracks','/api/player/uploads/local/history','/api/player/torrents','/api/player/youtube','/api/player/agent-queue'].includes(path)), []);
    await screenshot('form');
    await dialog.locator('#direct-audio-file-input').setInputFiles(['intro.wav','encore.wav'].map(name =>
        ({name, mimeType: 'audio/wav', buffer: Buffer.from('audio')})));
    await dialog.locator('#local-target-playlist').selectOption('7');
    await Promise.all([
        page.waitForResponse(res => res.request().headers()['x-furumusic-upload-complete'] === 'true'),
        dialog.getByRole('button', {name:'Upload selected files'}).click(),
    ]);
    assert.deepEqual(uploaded.map(h => h['x-furumusic-filename']), ['intro.wav', 'encore.wav']);
    assert.ok(uploaded[0]['x-furumusic-upload-batch-id']);
    assert.equal(uploaded[0]['x-furumusic-upload-batch-id'], uploaded[1]['x-furumusic-upload-batch-id']);
    assert.ok(uploaded.every(h => h['x-furumusic-target-playlist-id'] === '7'));
    await tab('youtube').click();
    const youtube = dialog.locator('#youtube-download-url');
    await youtube.fill('https://www.youtube.com/watch?v=demo');
    await youtube.press('Enter');
    await dialog.locator('#youtube-target-playlist').selectOption('7');
    await Promise.all([
        page.waitForResponse(res => new URL(res.url()).pathname === '/api/player/youtube/start'),
        dialog.getByRole('button', {name:'Start import'}).click(),
    ]);
    assert.deepEqual(videoStarted.selected_source_ids, ['clip']);
    assert.equal(videoStarted.target_playlist_id, 7);
    await tab('torrents').click();
    const magnet = dialog.locator('#torrent-magnet-input');
    await magnet.fill('magnet:?xt=urn:btih:0123456789012345678901234567890123456789');
    await dialog.locator('.torrent-preview-submit').click();
    const chooseFiles = dialog.locator('.torrent-choose-files');
    await chooseFiles.waitFor();
    assert.equal(await chooseFiles.getAttribute('aria-expanded'), 'false');
    assert.ok(await dialog.locator('.torrent-start').isEnabled());
    const tree = dialog.locator('.torrent-file-tree');
    assert.equal(await tree.count(), 0);
    await chooseFiles.click();
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
    await tree.evaluate(el => {el.scrollTop = 0;});
    await screenshot('torrent');
    await Promise.all([
        page.waitForResponse(res => new URL(res.url()).pathname === '/api/player/torrents/tour/start'),
        dialog.locator('.torrent-start').click(),
    ]);
    assert.equal(started?.target_playlist_id, 7);
    assert.deepEqual(started?.selected_files, files.slice(0, -1).map(file => file.index));
    await tab('history').click();
    await historyView.getByText('Festival recordings', {exact:true}).waitFor();
    assert.equal(await historyView.getByText(/intro\.wav|encore\.wav/).count(), 0);
    await screenshot('history');
    await Promise.all([
        page.waitForResponse(res => {
            const url = new URL(res.url());
            return url.pathname === historyPath && url.searchParams.get('source') === 'youtube' && url.searchParams.get('id') === 'show';
        }),
        historyView.getByRole('button', {name:'Late show', exact:true}).click(),
    ]);
    await dialog.locator('#downloads-youtube').getByText('Completed', {exact:true}).waitFor();
    await tab('history').click();
    for (const title of ['Festival recordings', 'Late show']) {
        const row = historyView.getByRole('listitem').filter({hasText:title});
        await row.getByRole('checkbox').check();
    }
    await historyView.getByRole('button', {name:/Remove from history/i}).click();
    assert.equal(hidden.length, 0, 'selection is not removed until confirmed');
    await dialog.getByRole('button', {name:'Remove', exact:true}).click();
    await historyView.getByText('Festival recordings', {exact:true}).waitFor({state:'hidden'});
    assert.deepEqual(hidden.sort((a,b) => a.id.localeCompare(b.id)), [{source:'files',id:'batch-one'},{source:'youtube',id:'show'}]);
    assert.equal(await historyView.getByText('Late show', {exact:true}).count(), 0);
    assert.match(await statsView.innerText(), /120\s+tracks/i);
    await historyView.getByRole('button', {name:/Tour recordings/}).click();
    await dialog.locator('.torrent-start').waitFor();
    assert.equal(await tree.count(), 0, 'opening a saved torrent shows the transfer summary');
    await page.keyboard.press('Escape');
    await dialog.waitFor({state: 'hidden'});
    await page.evaluate(() => Alpine.store('user').openSettings());
    const settings = page.locator('.user-settings-modal.player-dialog-surface');
    await settings.waitFor();
    assert.deepEqual(await modalStyle(settings), downloadStyle, 'both dialogs share panel, heading and close control');
    await settings.locator('.player-dialog-close').click();
    await settings.waitFor({state: 'hidden'});
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

// Render with FURU_PODCAST_RENDER_FIXTURE=1 cargo test renders_podcast_navigation_and_views.
const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = readFileSync('target/podcast-player-fixture.html', 'utf8');
const alpine = readFileSync('target/podcast-alpine.min.js', 'utf8');
const seeder = {title: 'Hours', artist_names: ['Singer'], release_title:'Dawn', release_id: 7,
    content_id:'b3:abc', track_id:3, listened_ms:180000, completed:true, device_name:'Browser'};
const overview = {
    artists: { title:'Singer', artist_id: 8, plays: 5, detail:'', artist_names:[] },
    tracks: { ...seeder, plays:5, detail:'Singer' },
    releases: { ...seeder, title:'Dawn', plays:5, detail:'Singer' },
};

async function run(browser, width, locale) {
    const page = await browser.newPage({viewport:{width, height:850}, timezoneId:'Europe/Madrid', locale});
    page.setDefaultTimeout(6000);
    const errors = [];
    const queries = [];
    let fail = false;
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', async route => {
        const url = new URL(route.request().url());
        const send = json => route.fulfill({json});
        if (url.hostname === 'listens.test' && url.pathname === '/') {
            return route.fulfill({contentType:'text/html', body: html.replace(/(<html[^>]*\blang=)"[^"]+"/, '$1"'+locale+'"')});
        }
        if (url.hostname === 'cdn.jsdelivr.net' && url.pathname.includes('alpine')) {
            return route.fulfill({contentType:'application/javascript', body: alpine});
        }
        if (url.pathname === '/api/player/listening') {
            const query = Object.fromEntries(url.searchParams);
            queries.push(query);
            if (fail) return route.fulfill({status:503,json:{}});
            if (query.view === 'overview') {
                return send({plays:5, tracks:1, listened_seconds:900, ranking:[overview[query.kind]],
                    daily:[{day:query.to,plays:5,listened_seconds:900}]});
            }
            const selected = query.from !== 'all';
            const date = selected ? query.from : '2026-03-28';
            return send({page:+query.page, limit:25, total:selected ? 1 : 26, items:[{...seeder,
                id:'event-'+query.page, title:'Hours '+query.page, started_at_ms: Date.parse(date+'T18:05:00Z')}]});
        }
        if (url.pathname === '/api/player/tracks-by-ids') {
            const ids = route.request().postDataJSON().ids;
            assert.deepEqual(ids, [3]);
            return send([{id:3,title:'Hours',duration_seconds:180,artists:[],featured_artists:[],stream_url:'/audio'}]);
        }
        if (url.pathname === '/api/player/artists') return send({items:[],total:0,has_more:false});
        return send([]);
    });
    await page.goto('http://listens.test/#listening');
    const content = page.locator('.my-listening');
    await content.locator('.listening-totals').waitFor();
    assert.equal(await content.locator('h1').innerText(), locale === 'ru' ? 'Мои прослушивания' : 'My listening');
    assert.equal(await content.locator('.listening-day').count(), 30);
    assert.equal(await content.getByRole('link',{name:'Singer',exact:true}).getAttribute('href'), '#artist/8');
    await content.locator('select').selectOption('7');
    await page.waitForFunction(() => document.querySelectorAll('.listening-day').length === 7);
    assert.equal(queries.at(-1).tz, 'Europe/Madrid');
    const expectedDay = queries.at(-1).to;
    await content.locator('.listening-rank-kinds button').nth(1).click();
    await content.locator('.listening-ranks button').waitFor();
    await page.evaluate(() => {Alpine.store('queue').playRelease = (tracks, idx) => {window.lastPlayed = tracks[idx].id;};});
    await content.locator('.listening-ranks button').click();
    await page.waitForFunction(() => window.lastPlayed === 3);
    await content.locator('.listening-rank-kinds button').nth(2).click();
    assert.equal(await content.getByRole('link',{name:'Dawn',exact:true}).getAttribute('href'), '#release/7');
    await content.locator('.listening-day:not(:disabled)').click();
    await page.waitForURL('**/#listening/history?day=' + expectedDay);
    await content.locator('.listening-history-row').waitFor();
    assert.equal(queries.at(-1).from, expectedDay);
    assert.equal(queries.at(-1).to, expectedDay);
    await content.locator('.listening-date-filter a').click();
    await page.waitForURL('**/#listening/history');
    await content.locator('.listening-pager:visible button').last().click();
    await content.getByRole('button',{name:'Hours 2',exact:true}).waitFor();
    await content.locator('details summary').click();
    const action = content.locator('details[open] button').first();
    assert.ok(await action.isVisible());
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    fail = true;
    await content.locator('.listening-tabs a').first().click();
    await content.locator('[role=alert]:visible').waitFor();
    fail = false;
    await content.locator('[role=alert] button').click();
    await content.locator('.listening-totals').waitFor();
    assert.deepEqual(errors, []);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({headless:true,...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {})});
    try {
        await run(browser, 1366, 'en');
        await run(browser, 320, 'ru');
        console.log('Personal listening overview, playback, day drill-down, history and retry passed on desktop and mobile.');
    } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });

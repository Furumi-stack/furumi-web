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
const daily = [
    {day:'2024-02-29',plays:1,listened_seconds:6000},
    {day:'2024-03-30',plays:1,listened_seconds:4000},
    {day:'2024-03-31',plays:8,listened_seconds:1800},
    {day:'2024-04-01',plays:16,listened_seconds:900},
    {day:'2024-04-02',plays:1,listened_seconds:80},
];

async function checkCalendar(page, queries, width, showEmpty) {
    const content = page.locator('.my-listening');
    const mode = name => content.locator('.listening-chart-switch [data-mode="' + name + '"]');
    const cell = day => content.locator('.listening-heatmap-day[data-day="' + day + '"]');
    const waitDays = count => page.waitForFunction(count =>
        document.querySelectorAll('.listening-heatmap-day').length === count, count);
    const waitFocused = day => page.waitForFunction(day => document.activeElement?.dataset.day === day, day);
    assert.equal(await mode('bars').getAttribute('aria-pressed'), 'true');
    const requests = queries.length;
    if (width < 600) await mode('heatmap').tap();
    else await mode('heatmap').click();
    await waitDays(7);
    assert.equal(await mode('heatmap').getAttribute('aria-pressed'), 'true');
    assert.equal(await content.locator('.listening-chart:visible').count(), 0);
    assert.equal(queries.length, requests, 'switching charts uses the loaded daily totals');
    assert.equal(await cell('2024-04-01').getAttribute('data-level'), '4');
    assert.equal(await cell('2024-03-28').getAttribute('data-level'), '0');
    assert.match(await cell('2024-04-01').getAttribute('aria-label'), /\b16\b/);
    const saturday = await cell('2024-03-30').boundingBox();
    const sunday = await cell('2024-03-31').boundingBox();
    const monday = await cell('2024-04-01').boundingBox();
    assert.ok(Math.abs(saturday.x - sunday.x) < 1 && sunday.y > saturday.y);
    assert.ok(monday.x > sunday.x && monday.y < saturday.y, 'the new week starts in the next column');
    assert.equal(queries.at(-1).to, '2024-04-02', 'period ends on local time after the DST transition');

    await content.locator('select').selectOption('365');
    await waitDays(365);
    assert.equal(queries.at(-1).from, '2023-04-04');
    assert.equal(await cell('2024-02-29').count(), 1);
    const viewport = await content.locator('.listening-heatmap-scroll').boundingBox();
    await page.waitForFunction(() => {
        const last = document.querySelector('.listening-heatmap-day[data-day="2024-04-02"]').getBoundingClientRect();
        const scroll = document.querySelector('.listening-heatmap-scroll').getBoundingClientRect();
        return last.left >= scroll.left - 1 && last.right <= scroll.right + 1;
    });
    assert.ok(viewport.x >= 0 && viewport.x + viewport.width <= width + 1);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    if (width >= 1366) {
        assert.ok(await content.locator('.listening-heatmap-scroll').evaluate(el => el.scrollWidth <= el.clientWidth + 1),
            'the whole year fits the desktop panel');
    }
    const tabbable = content.locator('.listening-heatmap-day[tabindex="0"]');
    assert.equal(await tabbable.count(), 1);
    await tabbable.focus();
    await page.keyboard.press('End');
    await waitFocused('2024-04-02');
    await page.keyboard.press('ArrowLeft');
    await waitFocused('2024-03-26');
    await page.keyboard.press('ArrowUp');
    await waitFocused('2024-03-25');
    await page.keyboard.press('Home');
    await page.keyboard.press('ArrowUp');
    await waitFocused('2023-04-04');

    const yearRequests = queries.length;
    await mode('bars').click();
    await page.waitForFunction(() => document.querySelectorAll('.listening-day').length === 365);
    assert.ok(await content.locator('.listening-chart').isVisible());
    await mode('heatmap').click();
    await waitDays(365);
    assert.equal(queries.length, yearRequests);
    await page.reload();
    await waitDays(30);
    assert.equal(await mode('heatmap').getAttribute('aria-pressed'), 'true');
    if (width < 600) await cell('2024-04-01').tap();
    else await cell('2024-04-01').click();
    await page.waitForURL('**/#listening/history?day=2024-04-01');
    await content.locator('.listening-history-row').waitFor();
    assert.equal(queries.at(-1).to, '2024-04-01');
    await page.goBack();
    await waitDays(30);
    await cell('2024-03-29').click();
    await page.waitForURL('**/#listening/history?day=2024-03-29');
    await content.locator('.listening-notice:visible').filter({hasText:/No plays on this day|В этот день/}).waitFor();

    showEmpty();
    await content.locator('.listening-tabs a').first().click();
    await page.waitForFunction(() => document.querySelectorAll('.listening-heatmap-day[data-level="0"]').length === 30);
    assert.equal(await mode('bars').isVisible(), true);
    await mode('bars').click();
    await page.reload();
    assert.equal(await mode('bars').getAttribute('aria-pressed'), 'true');
}

async function run(browser, width, locale) {
    const page = await browser.newPage({viewport:{width, height:850}, timezoneId:'Europe/Madrid', locale,
        hasTouch:width < 600, isMobile:width < 600});
    page.setDefaultTimeout(6000);
    await page.clock.setFixedTime(new Date('2024-04-01T22:30:00Z'));
    const errors = [];
    const queries = [];
    const coverRequests = [];
    let artistCover = '/api/player/cover/10/small';
    let fail = false;
    let empty = false;
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
        if (url.pathname.startsWith('/api/player/cover/')) {
            coverRequests.push(url.pathname);
            if (url.pathname === '/api/player/cover/11' || url.pathname === '/api/player/cover/12/small') {
                return route.fulfill({contentType:'image/svg+xml', body:'<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><circle cx="5" cy="5" r="5"/></svg>'});
            }
            return route.fulfill({status:404});
        }
        if (url.pathname === '/api/player/listening') {
            const query = Object.fromEntries(url.searchParams);
            queries.push(query);
            if (fail) return route.fulfill({status:503,json:{}});
            if (query.view === 'overview') {
                const item = query.kind === 'artists' ? {...overview.artists, cover_url:artistCover} : overview[query.kind];
                const rows = empty ? [] : daily.filter(row => row.day >= query.from && row.day <= query.to);
                return send({plays:rows.reduce((sum,row)=>sum + row.plays, 0), tracks:rows.length ? 1 : 0,
                    listened_seconds:rows.reduce((sum,row)=>sum + row.listened_seconds, 0), ranking:rows.length ? [item] : [], daily:rows});
            }
            const selected = query.from !== 'all';
            const date = selected ? query.from : '2026-03-28';
            if (selected && !daily.some(row => row.day === date)) return send({page:1,limit:25,total:0,items:[]});
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
    const portrait = content.locator('.listening-rank-art img');
    await portrait.scrollIntoViewIfNeeded();
    await page.waitForFunction(() => {
        const image = document.querySelector('.listening-rank-art img');
        return image?.getAttribute('src') === '/api/player/cover/10' && image.complete && !image.naturalWidth && getComputedStyle(image).opacity === '0';
    });
    assert.deepEqual(coverRequests, ['/api/player/cover/10/small','/api/player/cover/10']);
    for (const [id, suffix] of [[11, ''], [12, '/small']]) {
        artistCover = '/api/player/cover/'+id+'/small';
        await page.evaluate(() => Alpine.store('listening').load());
        await page.waitForFunction(path => {
            const image = document.querySelector('.listening-rank-art img');
            return image?.getAttribute('src') === path && image.naturalWidth && getComputedStyle(image).opacity === '1';
        }, '/api/player/cover/'+id+suffix);
    }
    assert.deepEqual(coverRequests.slice(2), ['/api/player/cover/11/small','/api/player/cover/11','/api/player/cover/12/small']);
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
    await content.locator('.listening-day:not(:disabled)').last().click();
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
    await checkCalendar(page, queries, width, () => {empty = true;});
    assert.deepEqual(errors, []);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({headless:true,...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {})});
    try {
        await run(browser, 1366, 'en');
        await run(browser, 320, 'ru');
        console.log('Personal listening overview, calendar heatmap, playback, day drill-down, history and retry passed on desktop and mobile.');
    } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });

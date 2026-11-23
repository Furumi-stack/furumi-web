// Render the player fixture with FURU_PODCAST_RENDER_FIXTURE=1 before running.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = readFileSync('target/podcast-player-fixture.html', 'utf8');
const alpine = readFileSync('target/podcast-alpine.min.js', 'utf8');
const likes = { id: -1, kind: 'likes', title: 'Likes', track_count: 14, is_own: true };
const owned = { id: 7, kind: 'user', title: 'Initial D', track_count: 3, is_own: true };
const pin = { id: 9, kind: 'release', title: 'Ocean Rain', href: '#release/9' };

async function checkCollection(browser) {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    const saved = { id: 8, kind: 'user', title: 'Shared mix', is_saved: true, is_public: true, track_count: 4 };
    const publicMix = { id: 9, kind: 'user', title: 'Public mix', is_public: true, track_count: 6 };
    const artist = {id: 42, name: 'Echo', track_count: 1};
    let pins = [];
    await page.route('**/*', route => {
        const {hostname, pathname} = new URL(route.request().url());
        if (hostname === 'sidebar.test' && pathname === '/') return route.fulfill({contentType: 'text/html', body: html});
        if (hostname === 'cdn.jsdelivr.net' && pathname.includes('alpine')) return route.fulfill({contentType: 'application/javascript', body: alpine});
        if (pathname === '/api/player/pins' && route.request().method() === 'POST') {
            const item = route.request().postDataJSON();
            pins = item.pinned ? [{...item, kind: 'playlist'}] : [];
        }
        const responses = {
            '/api/player/playlists': [likes, owned, saved, publicMix], '/api/player/pins': pins,
            '/api/player/me': {id: 1, name: 'Listener'}, '/api/player/follows': {artist_ids: [artist.id], artists: [artist]},
            '/api/player/artists': {items: [], total: 0, has_more: false},
        };
        return route.fulfill({json: responses[pathname] || []});
    });
    await page.goto('http://sidebar.test/#collection');
    const entries = page.locator('.collection-list');
    await entries.getByText(owned.title).waitFor();
    await entries.getByText(saved.title).waitFor();
    assert.equal(await entries.getByText(publicMix.title).count(), 0);
    const ownedRow = entries.locator('.collection-row').filter({hasText: owned.title});
    await ownedRow.locator('.collection-more').click();
    await ownedRow.getByRole('button', {name: 'Rename'}).waitFor();
    await page.keyboard.press('Escape');
    await ownedRow.locator('.collection-popup').waitFor({state: 'hidden'});
    const pinButton = entries.locator('.collection-row').filter({hasText: saved.title}).locator('.collection-pin');
    const sidebar = page.locator('.sidebar-left .music-shortcuts');
    await pinButton.click();
    await sidebar.getByText(saved.title).waitFor();
    assert.equal(await pinButton.getAttribute('aria-pressed'), 'true');
    await page.reload();
    await sidebar.getByText(saved.title).waitFor();
    await pinButton.click();
    await sidebar.getByText(saved.title).waitFor({state: 'hidden'});
    await page.locator('.collection-tabs a[href="#collection?tab=artists"]').click();
    await entries.getByText(artist.name).waitFor();
    assert.equal(await entries.locator('a').getAttribute('href'), '#artist/42');
    await page.locator('.sidebar-left a[href="#artists"]').click();
    await page.locator('.collection-tabs a[href="#artists?tab=playlists"]').click();
    await entries.getByText(publicMix.title).waitFor();
    assert.equal(await entries.getByText(owned.title).count(), 0);
    assert.deepEqual(errors, []);
    await page.close();
}

async function checkSidebar(browser, mobile) {
    const page = await browser.newPage({ viewport: { width: mobile ? 375 : 1280, height: 800 } });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
        const url = new URL(route.request().url());
        if (url.hostname === 'sidebar.test' && url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
        if (url.hostname === 'cdn.jsdelivr.net' && url.pathname.includes('alpine')) return route.fulfill({ contentType: 'application/javascript', body: alpine });
        const responses = {
            '/api/player/artists': { items: [], total: 0, has_more: false },
            '/api/player/me': { id: 1, name: 'Listener', role: mobile ? 'user' : 'admin' },
            '/api/player/playlists': [likes, owned], '/api/player/playlists/-1': { ...likes, tracks: [] },
            '/api/player/pins': mobile ? [pin] : [],
        };
        return route.fulfill({ json: responses[url.pathname] || [] });
    });
    await page.goto('http://sidebar.test/');
    const menuButton = page.locator('.mobile-library-btn');
    const nav = page.locator(mobile ? 'dialog .music-nav' : '.sidebar-left .music-nav');
    if (mobile) await menuButton.click();
    await nav.getByRole('link', { name: mobile ? pin.title : owned.title }).waitFor();
    assert.deepEqual(await nav.locator('.music-nav-pages a').allTextContents().then(values => values.map(value => value.trim())),
        ['Catalog', 'My music', 'Podcasts', 'My listening']);
    await nav.getByRole('link', { name: /Liked songs/ }).click();
    await page.waitForURL('**/#playlist/-1');
    await page.waitForFunction(() => Alpine.store('library').currentPlaylist?.id === -1);
    if (mobile) {
        await page.locator('dialog').waitFor({ state: 'detached' });
        await menuButton.click();
        await page.locator('dialog:modal').waitFor();
        await nav.getByRole('link', { name: /Liked songs/ }).click();
        await page.locator('dialog').waitFor({ state: 'detached' });
        assert.equal(new URL(page.url()).hash, '#playlist/-1');
        await menuButton.click();
        await page.locator('dialog:modal').waitFor();
        await page.keyboard.press('Escape');
        await page.locator('dialog').waitFor({ state: 'detached' });
        assert.ok(await menuButton.evaluate(element => element === document.activeElement));
    } else {
        await page.goto('http://sidebar.test/#artists?tab=playlists');
        await page.waitForFunction(() => Alpine.store('library').view === 'published');
        assert.equal(await nav.getByRole('link', { name: 'Catalog' }).getAttribute('aria-current'), 'page');
    }
    const trigger = page.locator(mobile ? '.mobile-account-chip' : '.sidebar-account-trigger');
    const panel = page.locator(mobile ? '#mobile-account-actions' : '#sidebar-account-actions');
    await trigger.click();
    await panel.getByRole('button', { name: 'Settings' }).waitFor();
    assert.equal(await panel.getByRole('link', { name: 'Admin Panel' }).count(), mobile ? 0 : 1);
    assert.equal(await panel.getByRole('link', { name: 'Log out' }).getAttribute('href'), '/logout');
    await page.keyboard.press('Escape');
    await panel.waitFor({ state: 'hidden' });
    await trigger.click();
    await panel.getByRole('button', { name: 'Settings' }).click();
    await page.locator('.user-settings-modal').waitFor();
    assert.equal(await panel.isVisible(), false);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    assert.deepEqual(errors, []);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({
        headless: true, ...(process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : {}),
    });
    try {
        await checkSidebar(browser, false);
        await checkSidebar(browser, true);
        await checkCollection(browser);
        console.log('Desktop and mobile navigation and account controls passed.');
    } finally {
        await browser.close();
    }
})().catch(error => { console.error(error); process.exitCode = 1; });

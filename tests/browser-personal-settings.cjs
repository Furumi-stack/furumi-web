// Render target/podcast-player-fixture.html with FURU_PODCAST_RENDER_FIXTURE=1 first.
const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = readFileSync('target/podcast-player-fixture.html', 'utf8');

async function checkSettings(browser, width, height) {
    const mobile = width < 600;
    const page = await browser.newPage({viewport: {width, height}});
    page.setDefaultTimeout(8000);
    if (height === 600) await page.emulateMedia({reducedMotion: 'reduce'});
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    let web = ['Firefox on Windows', 'Firefox on macOS'];
    let trusted = ['Living room', 'TUI-MacBook', 'Studio'];
    let pending = [];
    let lastfm = {configured: true, connected: true, username: 'longplay'};
    await page.route('**/*', route => {
        const {pathname} = new URL(route.request().url());
        if (pathname === '/') return route.fulfill({contentType: 'text/html', body: html});
        if (pathname.includes('alpine') && pathname.endsWith('.js')) {
            return route.fulfill({contentType: 'application/javascript', path: 'target/podcast-alpine.min.js'});
        }
        if (['/api/player/devices/poll','/api/player/devices/heartbeat'].includes(pathname)) {
            const current = route.request().postDataJSON().device_id;
            const devices = web.map((name, i) => ({
                id: i ? 'browser-' + i : current, name, is_current: i === 0, is_active: i === 0, kind: 'web',
            }));
            return route.fulfill({json: {devices, active_device_id: current, commands: [], jams: []}});
        }
        const responses = {
            '/api/player/me': {id: 1, name: 'Listener'},
            '/api/player/artists': {items: [], total: 0, has_more: false},
            '/api/player/lastfm/status': lastfm,
            '/api/player/fed-devices': {
                devices: trusted.map((name, i) => ({device_id: 'trusted-' + i, name, client_version: '0.6.0', is_self: i === 0})),
                active_devices: trusted.length, outbox_ops: 1000, unresolved_items: 1, pending,
            },
        };
        if (pathname === '/api/player/fed-devices/pairing') pending = [];
        return route.fulfill({json: responses[pathname] || []});
    });
    await page.goto('http://settings.test/');
    const trigger = page.locator(mobile ? '.mobile-account-chip' : '.sidebar-account-trigger');
    const account = page.locator(mobile ? '#mobile-account-actions' : '#sidebar-account-actions');
    await trigger.click();
    await account.getByRole('button', {name: 'Settings', exact: true}).click();
    const modal = page.locator('#user-settings-box');
    await page.waitForFunction(() => document.activeElement?.id === 'user-settings-title');
    if (!mobile) {
        const gap = await modal.locator('.offline-preferences').evaluate(panel =>
            panel.getBoundingClientRect().right - panel.querySelector('[role="switch"]').getBoundingClientRect().right);
        assert.ok(Math.abs(gap) <= 1, 'disabled cache switch aligns with the right edge of its panel');
    }
    await page.evaluate(() => Object.assign(Alpine.store('offline'), {
        supported: true, enabled: true, bytes: 126320000, limitBytes: 10e9,
        items: Array.from({length: 15}, (_, i) => ({id: i + 1, title: 'Song ' + (i + 1), artist: 'Artist', bytes: 8421333})),
    }));
    await modal.getByText('Firefox on Windows', {exact: true}).waitFor();
    await modal.getByText('TUI-MacBook', {exact: true}).waitFor({state: 'attached'});
    await modal.getByRole('button', {name: 'Disconnect', exact: true}).waitFor();
    assert.equal(await modal.getByRole('combobox', {name: 'Interface language'}).inputValue(), 'en');
    const cache = modal.locator('#open-offline-cache');
    await cache.waitFor();

    async function checkBounds() {
        const sizes = await modal.evaluate(el => ({
            panel: el.clientWidth, content: el.scrollWidth,
            left: el.getBoundingClientRect().left, right: el.getBoundingClientRect().right,
            height: el.getBoundingClientRect().height, scroll: el.scrollHeight,
        }));
        assert.ok(sizes.left >= 0 && sizes.right <= width + 1 && sizes.content <= sizes.panel + 1,
            'settings overflow at ' + width + 'px: ' + JSON.stringify(sizes));
        return sizes;
    }
    const initial = await checkBounds();
    if (!mobile && height >= 800) {
        assert.ok(initial.height < 680 && initial.scroll <= initial.height + 1, 'ordinary desktop settings should fit without scrolling');
        const preferences = await modal.locator('.settings-preference').evaluateAll(items => items.map(item => item.offsetTop));
        assert.equal(preferences[0], preferences[1], 'language and Last.fm share one row on desktop');
    }
    if (process.env.SETTINGS_SCREENSHOTS) {
        await page.screenshot({path: 'target/settings-' + width + 'x' + height + '.png', animations: 'disabled'});
    }
    await cache.click();
    await modal.locator('#offline-cache-back').click();
    await page.waitForFunction(() => document.activeElement?.id === 'open-offline-cache');

    if (mobile) {
        await modal.locator('.settings-device-tabs label').filter({has: page.locator('input[value="trusted"]')}).click();
        await modal.getByText('TUI-MacBook', {exact: true}).waitFor();
        assert.equal(await modal.getByText('Firefox on Windows', {exact: true}).isVisible(), false);
    }
    const syncInfo = modal.locator('.settings-device-info');
    await syncInfo.focus();
    const tip = modal.locator('#settings-sync-details');
    await tip.getByText('1000', {exact: true}).waitFor();
    const box = await tip.boundingBox();
    assert.ok(box.x >= 0 && box.x + box.width <= width, 'sync tooltip must be readable at ' + width);
    await syncInfo.press('Escape');
    await tip.waitFor({state: 'hidden'});
    assert.ok(await modal.isVisible(), 'dismissing tooltip keeps settings open');

    const pairing = modal.locator('.settings-pair-toggle');
    await pairing.press('Enter');
    await modal.locator('#settings-device-pairing input:not([readonly])').waitFor();
    assert.equal(await pairing.getAttribute('aria-expanded'), 'true');
    await pairing.press('Enter');
    assert.equal(await pairing.getAttribute('aria-expanded'), 'false');

    trusted = Array.from({length: 25}, (_, i) => i ? 'Device ' + i : 'StreamingLongDeviceName'.repeat(8));
    web = [...web, ...Array.from({length: 20}, (_, i) => 'Browser ' + i)];
    pending = [{request_id: 'join-1', device_id: 'new-device', name: 'Kitchen', client_version: '0.6.0'}];
    await page.evaluate(() => Promise.all([Alpine.store('devices').loadFedDevices(), Alpine.store('devices').poll()]));
    await modal.getByText('Device 24', {exact: true}).waitFor({state: 'attached'});
    await checkBounds();
    const scrollableLists = await modal.locator('.settings-device-list').evaluateAll(lists =>
        lists.filter(list => list.scrollHeight > list.clientHeight + 1 && /auto|scroll/.test(getComputedStyle(list).overflowY)).length);
    assert.ok(scrollableLists > 0, 'large device collections must be bounded');
    const request = modal.locator('.settings-device-request');
    await request.getByText('Kitchen', {exact: true}).waitFor();
    await request.getByRole('button', {name: 'Reject'}).click();
    await request.waitFor({state: 'hidden'});

    lastfm = {configured: true, connected: true, username: 'a_very_long_scrobbling_account'.repeat(4)};
    await page.evaluate(() => Alpine.store('user').loadLastfm());
    await modal.getByText(lastfm.username, {exact: true}).waitFor();
    await checkBounds();
    lastfm = {configured: false, connected: false};
    await page.evaluate(() => Alpine.store('user').loadLastfm());
    assert.ok(await modal.locator('.preference-action').isDisabled());
    lastfm = {configured: true, connected: true, reauth_required: true};
    await page.evaluate(() => Alpine.store('user').loadLastfm());
    await modal.getByRole('button', {name: 'Reconnect', exact: true}).waitFor();
    lastfm = {configured: true, connected: false};
    await page.evaluate(() => Alpine.store('user').loadLastfm());
    assert.ok(await modal.locator('.preference-action').isEnabled());

    await page.evaluate(() => {
        Alpine.store('offline').active = true;
        document.documentElement.classList.add('offline-active');
    });
    await modal.getByText('Devices are unavailable offline.', {exact: true}).waitFor();
    assert.equal(await pairing.isVisible(), false);
    await page.keyboard.press('Escape');
    await modal.waitFor({state: 'detached'});
    assert.ok(await trigger.evaluate(el => el === document.activeElement), 'closing returns focus to the account menu');
    assert.deepEqual(errors, []);
    console.log(width + 'x' + height, 'settings bounds', initial);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({headless: true, ...(process.env.CHROME_PATH ? {executablePath: process.env.CHROME_PATH} : {})});
    try {
        for (const [w, h] of [[1280, 800], [1280, 600], [390, 844], [320, 568]]) await checkSettings(browser, w, h);
    } finally {
        await browser.close();
    }
})().catch(error => { console.error(error); process.exitCode = 1; });

// Render with FURU_PODCAST_RENDER_FIXTURE=1 cargo test renders_podcast_navigation_and_views.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = readFileSync('target/podcast-player-fixture.html', 'utf8');
const alpine = readFileSync('target/podcast-alpine.min.js', 'utf8');
const artwork = '<svg xmlns="http://www.w3.org/2000/svg" width="640" height="640"><rect width="640" height="640" fill="green"/></svg>';
const localCover = '/api/player/cover/10/medium';

async function setTrack(page, id, cover) {
    await page.evaluate(([id, cover]) => {
        Alpine.store('player').currentTrack = {
            id, title: 'Track ' + id, artists: [], featured_artists: [], cover_url: cover,
        };
    }, [id, cover]);
}

async function setup(browser, options) {
    const page = await browser.newPage({ ...options, reducedMotion: 'reduce' });
    page.setDefaultTimeout(5000);
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
        const { pathname, hostname } = new URL(route.request().url());
        if (hostname === 'artwork.test' && pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
        if (hostname === 'cdn.jsdelivr.net' && pathname.includes('alpine')) return route.fulfill({ contentType: 'application/javascript', body: alpine });
        if (route.request().resourceType() === 'image') return route.fulfill({ contentType: 'image/svg+xml', body: artwork });
        if (pathname === '/api/player/artists') return route.fulfill({ json: { items: [], total: 0, has_more: false } });
        return route.fulfill({ json: [] });
    });
    await page.goto('http://artwork.test/');
    await page.waitForFunction(() => window.Alpine?.store('player'));
    await setTrack(page, 1, localCover);
    return { page, errors, cover: page.locator('button.player-cover'), preview: page.locator('.player-cover-preview') };
}

async function desktop(browser) {
    const { page, errors, cover, preview } = await setup(browser, { viewport: { width: 1366, height: 850 } });
    await preview.waitFor({ state: 'hidden' });
    await cover.hover();
    const enlarged = preview.locator('img[src="/api/player/cover/10/large"]');
    await enlarged.waitFor();
    const small = await cover.boundingBox(), large = await enlarged.boundingBox();
    assert.ok(large.width > small.width * 2 && large.height > small.height * 2);
    assert.ok(large.x >= 0 && large.y >= 0 && large.x + large.width <= 1366 && large.y + large.height <= 850);
    const outside = { x: 1350, y: 10 };
    await page.mouse.move(outside.x, outside.y);
    await preview.waitFor({ state: 'hidden' });

    await cover.hover();
    await cover.click();
    await page.mouse.move(outside.x, outside.y);
    await enlarged.click();
    await page.mouse.move(outside.x, outside.y);
    // An unpinned preview closes after the pointer has crossed the gap between the images.
    await page.waitForTimeout(300);
    assert.ok(await enlarged.isVisible(), 'pinned artwork survives pointer exit and clicks inside');

    const sharedCover = '/api/player/shares/test-token/cover/2';
    await setTrack(page, 2, sharedCover);
    await preview.locator('img[src="' + sharedCover + '"]').waitFor();
    await page.mouse.click(outside.x, outside.y);
    await preview.waitFor({ state: 'hidden' });
    await cover.focus();
    await page.keyboard.press('Enter');
    await preview.waitFor();
    await page.keyboard.press('Escape');
    await preview.waitFor({ state: 'hidden' });
    await cover.click();
    await preview.waitFor();
    await setTrack(page, 3, null);
    await preview.waitFor({ state: 'hidden' });
    assert.deepEqual(errors, []);
    await page.close();
}

async function mobile(browser) {
    const { page, errors, cover, preview } = await setup(browser, {
        viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true,
    });
    await cover.tap();
    await page.locator('.player-bar.mobile-expanded').waitFor();
    await preview.waitFor({ state: 'hidden' });
    await cover.tap();
    await preview.locator('img[src="/api/player/cover/10/large"]').waitFor();
    const bounds = await preview.boundingBox();
    assert.ok(bounds.x >= 0 && bounds.y >= 0 && bounds.x + bounds.width <= 390 && bounds.y + bounds.height <= 844);
    const outsideY = bounds.y > 2 ? 1 : Math.ceil(bounds.y + bounds.height) + 1;
    await page.touchscreen.tap(1, outsideY);
    await preview.waitFor({ state: 'hidden' });
    assert.deepEqual(errors, []);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({
        headless: true, ...(process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : {}),
    });
    try {
        await desktop(browser);
        await mobile(browser);
        console.log('Player artwork hover, pin, dismissal, track changes and touch checks passed.');
    } finally {
        await browser.close();
    }
})().catch(error => { console.error(error); process.exitCode = 1; });

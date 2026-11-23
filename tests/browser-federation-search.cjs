const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = fs.readFileSync('target/podcast-player-fixture.html', 'utf8');
const alpine = fs.readFileSync('target/podcast-alpine.min.js', 'utf8');
function stream(events) {
    return events.map(([kind, entity], index) => {
        const data = JSON.stringify({ kind, sequence: index + 1, entity });
        return `event: ${kind}\ndata: ${data}\n\n`;
    }).join('');
}

function track(id, title) {
    return {
        key: { content_id: `b3:${String(id).padStart(64, '0')}` },
        metadata: { title, artists: [{ name: 'Band' }], duration_seconds: 180 + id },
        availability: { state: 'federated', federation: [{ owner: 'peer', item_id: String(id) }] },
    };
}

const trackCount = 120;
const groupCount = 6;
const burst = stream([
    ...Array.from({ length: trackCount }, (_, id) => ['federation.track', track(id, 'Song ' + (id % groupCount))]),
    ['federation.artist', { key: { normalized_name: 'band' }, name: 'Band', peers: ['peer'] }],
    ['federation.release', {
        key: { normalized_title: 'album', primary_artists: ['band'], year: 2000 },
        title: 'Album', artists: ['Band'], year: 2000, cover_url: '/test/missing-cover', sources: [],
    }],
    ['search.completed', { partial: false }],
]);

(async () => {
    const browser = await chromium.launch({
        headless: true, ...(process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : {}),
    });
    try {
        const page = await browser.newPage({ viewport: { width: 1200, height: 900 } });
        const errors = [];
        page.on('pageerror', error => errors.push(error.message));
        await page.addInitScript(() => {
            window.EventSource = class extends EventSource {
                constructor(url, options) {
                    super(url, options);
                    if (url.startsWith('/api/player/federation/search/events?')) window.searchStream = this;
                }
            };
        });
        let sendBurst, sendOnLeave;
        const responses = new Map([
            ['Song', new Promise(resolve => { sendBurst = resolve; })],
            ['leave', new Promise(resolve => { sendOnLeave = resolve; })],
            ['partial', stream([
                ['federation.track', track(1, 'Other song')],
                ['federation.track', track(2, 'Partial match')],
                ['federation.track', track(3, 'Provisional title')],
                ['federation.track', track(3, 'Partial')],
                ['search.failed', { message: 'Peer disconnected' }],
            ])],
        ]);
        await page.route('**/*', async route => {
            const url = new URL(route.request().url());
            if (url.hostname === 'groups.test' && url.pathname === '/') {
                return route.fulfill({ contentType: 'text/html', body: html });
            }
            if (url.hostname === 'cdn.jsdelivr.net' && url.pathname.includes('alpine')) {
                return route.fulfill({ contentType: 'application/javascript', body: alpine });
            }
            if (url.pathname === '/api/player/search') {
                return route.fulfill({ json: { tracks: [], artists: [], releases: [], podcasts: [] } });
            }
            if (url.pathname === '/api/player/artists') {
                return route.fulfill({ json: { items: [], total: 0, has_more: false } });
            }
            if (url.pathname === '/api/player/federation/search/events') {
                const body = await responses.get(url.searchParams.get('q'));
                return route.fulfill({ contentType: 'text/event-stream', body });
            }
            if (url.pathname === '/test/local-artist') return route.fulfill({
                contentType: 'image/svg+xml', body: '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>',
            });
            if (route.request().resourceType() === 'image') return route.fulfill({ status: 404, body: '' });
            return route.fulfill({ json: [] });
        });
        await page.goto('http://groups.test/');
        await page.waitForFunction(() => window.Alpine?.store('library'));
        await page.evaluate(async () => {
            const library = Alpine.store('library');
            window.imageErrors = 0;
            document.addEventListener('error', event => {
                if (event.target instanceof HTMLImageElement && event.target.closest('.federation-entity-card')) {
                    window.imageErrors++;
                }
            }, true);
            library.searchQuery = 'Song';
            await library.search('Song');
            window.trackUpdates = 0;
            Alpine.effect(() => {
                void library.federationSearch.tracks;
                window.trackUpdates++;
            });
            window.started = performance.now();
        });
        sendBurst(burst);
        await page.waitForFunction(() => !Alpine.store('library').federationSearch.loading);
        const first = await page.evaluate(() => ({
            elapsed: Math.round(performance.now() - window.started),
            updates: window.trackUpdates,
            sourceTracks: Alpine.store('library').federationSearch.tracks.length,
            mountedRows: document.querySelectorAll('[data-search-source="federation"]').length,
        }));
        await page.waitForFunction(() => window.imageErrors >= 3);
        // Allow several turns of the event loop after the last valid fallback has failed.
        await page.waitForTimeout(400);
        const final = await page.evaluate(() => ({ errors: window.imageErrors, updates: window.trackUpdates }));
        console.log('Federation search burst', { ...first, imageErrors: final.errors, settledUpdates: final.updates });
        assert.equal(first.sourceTracks, trackCount);
        assert.equal(first.mountedRows, groupCount);
        assert.equal(await page.locator('.federation-entity-card img').count(), 0);
        assert.equal(final.errors, 3, 'each of the three missing artwork URLs is tried once');
        assert.ok(first.updates < trackCount / 2, 'the event stream is published in batches');
        assert.equal(final.updates, first.updates, 'artwork failures do not invalidate track results');

        // A local image arriving after the missing federation image still gets a chance to load.
        await page.evaluate(() => {
            Alpine.store('library').searchResults.artists = [{ id: 1, name: 'Band', image_url: '/test/local-artist' }];
        });
        await page.waitForSelector('.federation-entity-card img.artwork-loaded[src="/test/local-artist"]');

        // A failing stream publishes its last batch, including replacements for existing content IDs.
        await page.evaluate(() => Alpine.store('library').search('partial'));
        await page.waitForFunction(() => Alpine.store('library').federationSearch.error === 'Peer disconnected');
        assert.deepEqual(await page.locator('[data-search-source="federation"] .search-track-name').allTextContents(),
            ['Partial', 'Partial match', 'Other song']);

        // Leave after the first SSE event has been buffered but before its batch is published.
        await page.evaluate(async () => {
            const library = Alpine.store('library');
            await library.search('leave');
            window.searchStream.addEventListener('federation.track', () => library.goArtists(), { once: true });
        });
        sendOnLeave(burst);
        await page.waitForFunction(() => Alpine.store('library').view === 'artists');
        await page.waitForTimeout(100);
        assert.equal(await page.evaluate(() => window.searchStream.readyState === EventSource.CLOSED), true);
        assert.deepEqual(await page.evaluate(() => Alpine.store('library').federationSearch),
            { loading: false, error: '', artists: [], releases: [], tracks: [] });
        assert.deepEqual(errors, []);
    } finally {
        await browser.close();
    }
})().catch(error => { console.error(error); process.exitCode = 1; });

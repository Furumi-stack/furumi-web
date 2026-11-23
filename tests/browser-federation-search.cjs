const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = fs.readFileSync('target/podcast-player-fixture.html', 'utf8');
const alpine = fs.readFileSync('target/podcast-alpine.min.js', 'utf8');
function stream(events) {
    return events.map(([kind, entity, score], index) => {
        const data = JSON.stringify({ kind, sequence: index + 1, entity, score });
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
        let sendBurst, sendOnLeave, sendLocal, sendRemote;
        const delayedLocal = new Promise(resolve => { sendLocal = resolve; });
        const canonicalTrack = track(442, 'Song title');
        canonicalTrack.availability.local = { track_id: 42 };
        const canonicalArtist = { key: { normalized_name: 'song band' }, name: 'Song Band', peers: ['peer'], local_id: 7 };
        const canonicalRelease = { key: { normalized_title: 'album', primary_artists: ['band'], year: 2014 }, local_id: 442,
            title: 'Album', artists: ['Band'], sources: [{ owner: 'peer' }] };
        const canonicalEvents = stream([
            ['federation.track', canonicalTrack, 800], ['federation.track', track(443, 'Remote exact title'), 1000],
            ['federation.artist', canonicalArtist, 800], ['federation.release', canonicalRelease, 800],
            ['search.completed', {}],
        ]);
        const localResult = {
            artists: [{ id: 7, name: 'Song Band', image_url: '/test/local-artist' }],
            releases: [{ id: 442, title: 'Album', cover_url: '/test/local-cover' }],
            tracks: [{ id: 42, title: 'Song title', artists: [{ id: 7, name: 'Song Band' }], release_id: 442, release_title: 'Album' }],
            podcasts: [], scores: { tracks: { 42: 900 } },
        };
        const responses = new Map([
            ['Song', new Promise(resolve => { sendBurst = resolve; })],
            ['leave', new Promise(resolve => { sendOnLeave = resolve; })],
            ['late-local', canonicalEvents],
            ['early-local', new Promise(resolve => { sendRemote = resolve; })],
            ['partial', stream([
                ['federation.track', track(2, 'A partial match'), 950],
                ['federation.track', track(3, 'Partial (live)'), 950],
                ['federation.track', track(3, 'Partial'), 1000],
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
                const q = url.searchParams.get('q');
                if (q === 'late-local') await delayedLocal;
                if (q === 'early-local' || q === 'late-local') return route.fulfill({ json: localResult });
                return route.fulfill({ json: { tracks: [], artists: [], releases: [], podcasts: [] } });
            }
            if (url.pathname === '/api/player/artists') {
                return route.fulfill({ json: { items: [], total: 0, has_more: false } });
            }
            if (url.pathname === '/api/player/federation/search/events') {
                const body = await responses.get(url.searchParams.get('q'));
                return route.fulfill({ contentType: 'text/event-stream', body });
            }
            if (url.pathname === '/test/local-artist' || url.pathname === '/test/local-cover') return route.fulfill({
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
                if (event.target instanceof HTMLImageElement && event.target.closest('.search-federation-only')) {
                    window.imageErrors++;
                }
            }, true);
            library.searchQuery = 'Song';
            await library.search('Song');
            window.trackUpdates = 0;
            Alpine.effect(() => {
                void library.unifiedSearch.tracks;
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
        assert.equal(await page.locator('.search-federation-only img').count(), 0);
        assert.equal(await page.locator('.search-artist-card.search-federation-only:visible').count(), 1);
        assert.equal(await page.locator('.search-release-card.search-federation-only:visible').count(), 1);
        const released = page.locator('.search-release-card.search-federation-only').first();
        const visual = await released.evaluate(card => {
            const style = getComputedStyle(card);
            const corner = getComputedStyle(card, '::before');
            return { radius: style.borderTopLeftRadius, corner: corner.clipPath, color: corner.backgroundColor,
                duration: style.animationDuration, pointer: corner.pointerEvents };
        });
        assert.equal(visual.radius, '0px');
        assert.match(visual.corner, /^polygon\(/);
        const [red, green, blue] = visual.color.match(/\d+/g).map(Number);
        assert.ok(green > red && green > blue, 'remote-only corner uses the green accent');
        for (const selector of ['.search-artist-card', '.search-release-card', '.track-row']) {
            const png = await page.locator(selector + '.search-federation-only').first().screenshot({ animations: 'disabled' });
            const samples = await page.evaluate(async base64 => {
                const img = new Image();
                img.src = 'data:image/png;base64,' + base64;
                await img.decode();
                const canvas = document.createElement('canvas');
                const context = canvas.getContext('2d');
                context.drawImage(img, 0, 0);
                return [[0, 0], [1, 1], [1, 10], [10, 1], [14, 14]].map(([x, y]) =>
                    Array.from(context.getImageData(x, y, 1, 1).data).slice(0, 3));
            }, png.toString('base64'));
            // The outermost pixel can straddle a fractional row boundary.
            const [r, g, b] = samples[0];
            assert.ok(g > r && g > b && g > green * .5, selector);
            assert.deepEqual(samples.slice(1, 4), Array(3).fill([red, green, blue]), selector);
            assert.notDeepEqual(samples[4], [red, green, blue], 'the mark is triangular');
        }
        assert.equal(visual.pointer, 'none');
        assert.ok(parseFloat(visual.duration) > 0 && parseFloat(visual.duration) <= .15);
        await page.emulateMedia({ reducedMotion: 'reduce' });
        assert.equal(await released.evaluate(card => getComputedStyle(card).animationName), 'none');
        await page.emulateMedia({ reducedMotion: 'no-preference' });
        assert.equal(await page.locator('.federation-search-section:visible').count(), 0);
        assert.equal(final.errors, 3, 'each of the three missing artwork URLs is tried once');
        assert.ok(first.updates < trackCount / 2, 'the event stream is published in batches');
        assert.equal(final.updates, first.updates, 'artwork failures do not invalidate track results');

        // The local HTTP request and the SSE response can complete in either order.
        for (const query of ['late-local', 'early-local']) {
            await page.evaluate(q => { window.pendingSearch = Alpine.store('library').search(q); }, query);
            if (query === 'late-local') {
                await page.waitForFunction(() => !Alpine.store('library').federationSearch.loading);
            } else {
                await page.waitForFunction(() => !Alpine.store('library').searchLoading);
            }
            await page.locator('[data-search-source="local"]').waitFor();
            await page.evaluate(() => { window.canonicalRow = document.querySelector('[data-search-source="local"]'); });
            if (query === 'late-local') sendLocal(); else sendRemote(canonicalEvents);
            await page.evaluate(() => window.pendingSearch);
            await page.waitForFunction(() => !Alpine.store('library').federationSearch.loading);
            assert.equal(await page.locator('[data-search-source="local"]').count(), 1);
            assert.equal(await page.evaluate(() => document.querySelector('[data-search-source="local"]') === window.canonicalRow), true);
            assert.deepEqual(await page.locator('[data-search-source] .search-track-name:visible').allTextContents(),
                ['Remote exact title', 'Song title'], 'ranking combines both sources');
            assert.equal(await page.locator('[data-search-source="federation"]').count(), 1, 'another recording survives');
            assert.equal(await page.locator('.search-release-card:visible').count(), 1);
            assert.equal(await page.locator('.search-artist-card:visible').count(), 1);
            assert.equal(await page.locator('.search-release-card.search-federation-only:visible').count(), 0);
            assert.equal(await page.locator('[data-search-source="local"] .federation-source-count:visible').count(), 0);
            await page.waitForSelector('.search-release-card img.artwork-loaded[src="/test/local-cover"]');
        }

        // A failing stream publishes its last batch, including replacements for existing content IDs.
        await page.evaluate(() => Alpine.store('library').search('partial'));
        await page.waitForFunction(() => Alpine.store('library').federationSearch.error === 'Peer disconnected');
        assert.deepEqual(await page.locator('[data-search-source="federation"] .search-track-name').allTextContents(),
            ['Partial', 'A partial match']);

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
        await verifyReleaseNavigation(page);
        await verifyArtistListen(page);
        assert.deepEqual(errors, []);
    } finally {
        await browser.close();
    }
})().catch(error => { console.error(error); process.exitCode = 1; });

async function verifyReleaseNavigation(page) {
    const local = {
        id: 442, title: 'Вино из цикуты', artists: [{ id: 21, name: 'Саша Скул' }], year: 2014,
        release_type: 'album', cover_url: '/test/local-cover', tracks: [], uploaders: [],
    };
    function releaseUrl(title, name, year) {
        const key = { normalized_title: title, primary_artists: [name], release_type: null, year };
        return `http://groups.test/#releasefed?key=${encodeURIComponent(JSON.stringify(key))}`;
    }
    await page.route('**/api/player/releases/442', route => route.fulfill({ json: local }));
    await page.route('**/api/player/federation/release/resolve?*', route => {
        const key = JSON.parse(new URL(route.request().url()).searchParams.get('key'));
        const matched = key.normalized_title === 'вино из цикуты'
            && key.primary_artists.join() === 'саша скул' && key.year === 2014;
        return route.fulfill({ json: { local_id: matched ? 442 : null } });
    });
    const remote = { name: 'AC/DC', releases: [{
        title: 'Across Borders', year: 2017, release_type: 'album',
        tracks: [{ title: 'Remote song', item_id: 'abc', content_id: 'content-id', artists: ['AC/DC'] }],
    }] };
    await page.route('**/api/player/federation/artists/events?*', route => {
        const name = new URL(route.request().url()).searchParams.get('name');
        const entity = name === 'ac dc' ? remote : {
            name: local.artists[0].name, releases: [{ title: local.title, year: local.year, release_type: 'album', tracks: [] }],
        };
        const payload = JSON.stringify({ peer: 'peer', entity });
        return route.fulfill({ contentType: 'text/event-stream', body:
            `event: federation.artist.catalog\ndata: ${payload}\n\nevent: artist.completed\ndata: {}\n\n` });
    });

    // Leave the application so no in-memory search result can help with either lookup.
    await page.goto('about:blank');
    await page.goto(releaseUrl('вино из цикуты', 'саша скул', 2014));
    await page.waitForURL('**/#release/442');
    await page.locator('.release-cover img[src="/test/local-cover"]').waitFor();
    await page.reload();
    await page.waitForFunction(() => Alpine.store('library').currentRelease?.id === 442);

    const remoteUrl = releaseUrl('across borders', 'ac dc', 2017);
    await page.goto(remoteUrl);
    await page.getByText('Remote song', { exact: true }).first().waitFor();
    assert.equal(page.url(), remoteUrl);
    await page.reload();
    await page.getByText('Remote song', { exact: true }).first().waitFor();
    assert.equal(await page.locator('.release-artists a').first().innerText(), 'AC/DC');
    await page.goBack();
    await page.waitForURL('**/#release/442');
    await page.locator('.release-cover img[src="/test/local-cover"]').waitFor();
}

async function verifyArtistListen(page) {
    const feature = {
        id: 37, content_id: 'diagnosis', title: 'Диагноз', release_id: 87, release_title: '13/87',
        stream_url: '/api/player/stream/37', duration_seconds: 120,
        artists: [{ id: 7, name: 'Трагедия Всей Жизни' }], featured_artists: [{ id: 23, name: 'Кизиль' }],
    };
    const artist = {
        id: 23, name: 'Кизиль', releases: [], top_tracks: [], featured_tracks: [feature],
        total_track_count: 1, total_play_count: 0, track_content_ids: [feature.content_id],
    };
    await page.route('**/api/player/artists/23', route => route.fulfill({ json: artist }));
    await page.route('**/api/player/federation/artists/events?*', route =>
        route.fulfill({ contentType: 'text/event-stream', body: 'event: artist.completed\ndata: {}\n\n' }));
    await page.goto('about:blank');
    await page.goto('http://groups.test/#artist/23');
    await page.locator('.artist-appearance-row').waitFor();
    const stats = page.locator('.artist-stats');
    await stats.getByText('0 releases', { exact: true }).waitFor();
    await stats.getByText('1 track', { exact: true }).waitFor();
    await page.evaluate(() => {
        Alpine.store('player')._playLocal = track => { window.playedStream = track.stream_url; };
    });
    const listen = page.getByRole('button', { name: 'Listen', exact: true });
    await listen.click();
    const queue = () => page.evaluate(() => Alpine.store('queue').tracks.map(track => track.id));
    assert.deepEqual(await queue(), [37]);
    assert.equal(await page.evaluate(() => window.playedStream), feature.stream_url);

    // A local appearance and its peer copy join the ranked queue only once.
    await page.evaluate(() => {
        const library = Alpine.store('library');
        const [feature] = library.currentArtist.featured_tracks;
        library.currentArtist.top_tracks = [{ ...feature, id: 12, content_id: 'first' }, feature];
        library.artistFederation.tracks = ['diagnosis', 'remote'].map(id => library.federationTrackFromWire({
            title: id, owner: 'peer', item_id: id, content_id: id, artist_names: ['Кизиль'],
        }));
    });
    await listen.click();
    assert.deepEqual(await queue(), [12, 37, 'fed:remote']);
    await page.evaluate(() => {
        const library = Alpine.store('library');
        library.currentArtist.top_tracks = [];
        library.currentArtist.featured_tracks = [];
        library.playFederatedTrack = (track, options) => {
            window.peerPlayback = [track.key.content_id, options.queueIndex];
        };
    });
    await listen.click();
    assert.deepEqual(await queue(), ['fed:diagnosis', 'fed:remote']);
    assert.deepEqual(await page.evaluate(() => window.peerPlayback), ['diagnosis', 0]);
    await page.evaluate(() => { Alpine.store('library').artistFederation.tracks = []; });
    assert.equal(await listen.isDisabled(), true);
}

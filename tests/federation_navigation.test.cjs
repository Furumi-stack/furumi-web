const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { test } = require('node:test');
const vm = require('node:vm');

const template = readFileSync(`${__dirname}/../templates/player/scripts.html`, 'utf8');
const start = template.indexOf("Alpine.store('library', {");
const librarySource = template.slice(start, template.indexOf('\n    });', start) + 8);
const flush = () => new Promise(resolve => setImmediate(resolve));
const json = value => ({ ok: true, json: async () => value });

function setup(hash, fetch) {
    const location = { hash };
    const replacements = [];
    const streams = [];
    const offline = {active: false, manual: false};
    let library;
    class EventSource {
        constructor(url) { this.url = url; this.events = {}; streams.push(this); }
        addEventListener(kind, fn) { this.events[kind] = fn; }
        emit(kind, data) { this.events[kind]({ data: JSON.stringify(data) }); }
        close() { this.closed = true; }
    }
    vm.runInNewContext(librarySource, {
        location, fetch, EventSource, GUEST: false, playerOffline: () => offline.active, URLSearchParams, clearTimeout, setTimeout,
        window: {setTimeout, clearTimeout},
        Alpine: { store(name, value) {
            if (value) library = value;
            if (name === 'offline') return offline;
            if (name === 'library') return library;
        } },
        history: { replaceState(_state, _title, url) { location.hash = url; replacements.push(url); } },
    });
    library._scrollElement = () => null;
    library._afterNavigation = () => {};
    return { library, location, replacements, streams, offline };
}

function key(title, artist, year) {
    return { normalized_title: title, primary_artists: [artist], release_type: null, year };
}
const link = releaseKey => '#releasefed?key=' + encodeURIComponent(JSON.stringify(releaseKey));

test('federation closes subscriptions and pending search batches when switching offline', async () => {
    const {library, streams, offline} = setup('#artist/1', () => assert.fail('no HTTP request expected'));
    const release = {title:'Album', artists:[{name:'Band'}], tracks:[]};
    const startStreams = () => {
        library.startArtistFederation('Band');
        library.startReleaseFederation(release);
        library.startFederationSearch('Band');
    };
    startStreams();
    const [artist, edition, search] = streams;
    const match = {entity:{key:{normalized_name:'band'}, name:'Band'}, score:1};
    search.emit('federation.artist', match);
    offline.manual = offline.busy = true; // The page closes these before awaiting the worker acknowledgement.
    library.stopFederation();
    startStreams();
    assert.equal(streams.length, 3);
    assert.ok(streams.every(stream => stream.closed));
    offline.active = true;
    offline.manual = offline.busy = false;
    startStreams();
    assert.equal(streams.length, 3);
    offline.active = false;
    startStreams();
    assert.equal(streams.length, 6);
    // Queued events from the previous subscription must not affect the recovered page.
    artist.emit('federation.artist.catalog', {entity:{name:'Old'}});
    edition.emit('artist.completed', {});
    search.emit('federation.artist', match);
    search.onerror();
    await new Promise(resolve => setTimeout(resolve, 65));
    assert.equal(library.currentArtist, null);
    assert.equal(library.releaseFederationLoading, true);
    assert.equal(library.federationSearch.artists.length, 0);
    assert.equal(library.federationSearch.loading, true);
    library.stopFederation();
});

test('guest appearances add recordings, not owned releases, even across several catalogues', () => {
    const { library, streams } = setup('#artist/333', () => assert.fail('no request expected'));
    const wire = id => ({ item_id: id, content_id: id, title: 'Диагноз', artists: ['Трагедия Всей Жизни'], featured_artists: ['Кизиль'] });
    const appearance = id => ({ release_title: '13/87', year: 2014, release_type: 'album', track: wire(id) });
    library.currentArtist = { name: 'Кизиль', releases: [], total_track_count: 1,
        track_content_ids: ['local'], top_tracks: [], featured_tracks: [{ content_id: 'local' }] };
    library.startArtistFederation('Кизиль');
    const emit = (peer, ids) => streams[0].emit('federation.artist.catalog', {
        peer, entity: { name: 'Кизиль', releases: [], appears_on: ids.map(appearance) },
    });
    emit('first', ['local']);
    assert.equal(library.artistFederation.releases.length, 0);
    assert.equal(library.artistFederation.tracks.length, 0);
    assert.equal(library.artistTrackCount(), 1);
    emit('first', ['new-version']);
    emit('second', ['local', 'new-version']);
    assert.equal(library.artistFederation.releases.length, 0);
    assert.equal(library.artistTrackCount(), 2);
    assert.equal(library.artistFederation.tracks[0].availability.federation.length, 2);
    library.currentArtist = { name: 'Кизиль', releases: [], total_track_count: 0, featured_tracks: [] };
    library.startArtistFederation('Кизиль');
    streams[1].emit('federation.artist.catalog', { peer: 'first', entity: { appears_on: [appearance('remote')] } });
    assert.equal(library.artistFederation.releases.length, 0);
    assert.equal(library.artistTrackCount(), 1);
});

test('existing release deep links replace their own history entry with the local edition', async () => {
    for (const [id, title, artist, year] of [
        [442, 'вино из цикуты', 'саша скул', 2014],
        [1364, 'рэп из под стола remastered', 'гнойный', 2013],
    ]) {
        const releaseKey = key(title, artist, year);
        const calls = [];
        const { library, replacements } = setup(link(releaseKey), async input => {
            const url = new URL(input, 'https://player.test');
            calls.push(url.pathname);
            if (url.pathname.endsWith('/resolve')) {
                assert.deepEqual(JSON.parse(url.searchParams.get('key')), releaseKey);
                return json({ local_id: id });
            }
            return json({ id, title, year, artists: [{ id: 1, name: artist }], tracks: [], cover_url: '/covers/local' });
        });
        library._navigateFromHash({ fromHash: true });
        await flush();
        assert.deepEqual(calls, ['/api/player/federation/release/resolve', '/api/player/releases/' + id]);
        assert.deepEqual(replacements, ['#release/' + id]);
        assert.equal(library.currentRelease.cover_url, '/covers/local');
        assert.equal(library.currentRelease.id, id);
    }
});

test('a cold remote release loads its edition from a peer and rejects other years and types', async () => {
    const releaseKey = key('some album', 'ac dc', 1990);
    const { library, streams, location, replacements } = setup(link(releaseKey), () => json({ local_id: null }));
    library._navigateFromHash({ fromHash: true });
    await flush();
    const [source] = streams;
    assert.equal(location.hash, link(releaseKey));
    assert.equal(library.releaseFederationLoading, true);
    const candidate = (year, type, id) => ({ title: 'Some Album', year, release_type: type,
        tracks: [{ title: id, content_id: id, item_id: id, artists: ['AC/DC'] }] });
    const catalog = (...releases) => source.emit('federation.artist.catalog', { peer: 'peer', entity: { name: 'AC/DC', releases } });
    catalog(candidate(1992, 'album', 'wrong-year'));
    catalog(candidate(1990, 'album', 'ambiguous-album'), candidate(1990, 'ep', 'ambiguous-ep'));
    assert.equal(library.currentRelease, null);
    catalog(candidate(1990, 'album', 'right'));
    source.emit('federation.artist.catalog', {
        peer: 'second', entity: { name: 'AC/DC', releases: [candidate(1990, 'album', 'right')] },
    });
    catalog(candidate(1990, 'ep', 'wrong-type'));
    catalog(candidate(1990, 'album', 'right'));
    source.emit('artist.completed', {});
    assert.deepEqual(replacements, []);
    assert.equal(library.currentRelease.title, 'Some Album');
    assert.equal(library.currentRelease.artists[0].name, 'AC/DC');
    assert.deepEqual(Array.from(library.currentRelease.tracks, row => row.content_id), ['right']);
    assert.deepEqual(Array.from(library.currentRelease.federationPeers).sort(), ['peer', 'second']);
    assert.equal(library.currentRelease.tracks[0].release_key.release_type, 'album');
    assert.equal(library.releaseUnavailable, false);
    assert.equal(source.closed, true);
});

test('published release keys preserve protocol spelling and include all primary artists', () => {
    const { library } = setup('#artists', () => assert.fail('no request expected'));
    const releaseKey = library.federationReleaseKey('РЭП – Ремастер!', ['Гнойный', 'Björk', 'BJÖRK'], 2013, 'EP');
    assert.deepEqual(JSON.parse(JSON.stringify(releaseKey)), {
        normalized_title: 'рэп ремастер', primary_artists: ['björk', 'гнойный'], release_type: 'ep', year: 2013,
    });
});

test('unavailable and malformed release links stay on the requested page', async () => {
    for (const hash of ['#releasefed?key=%XX', link(key('unavailable', 'peer', 2002))]) {
        const { library, location, streams } = setup(hash, () => json({ local_id: null }));
        library._navigateFromHash({ fromHash: true });
        await flush();
        streams[0]?.emit('artist.completed', {});
        assert.equal(library.view, 'release_detail');
        assert.equal(library.currentRelease, null);
        assert.equal(library.releaseUnavailable, true);
        assert.equal(location.hash, hash);
    }
});

test('late resolution cannot redirect away from a subsequent artist deep link', async () => {
    let resolveRelease;
    const { library, location, replacements } = setup(link(key('release', 'artist', 2000)), async input => {
        if (input.includes('/release/resolve')) return new Promise(resolve => { resolveRelease = resolve; });
        if (input.includes('/artist/resolve')) return json({ local_id: 37 });
        return json({ id: 37, name: 'Artist', releases: [], image_url: '/artists/37' });
    });
    library._navigateFromHash({ fromHash: true });
    location.hash = '#artistfed?name=Artist';
    library._navigateFromHash({ fromHash: true });
    await flush();
    resolveRelease(json({ local_id: 442 }));
    await flush();
    assert.equal(library.currentArtist.id, 37);
    assert.deepEqual(replacements, ['#artist/37']);
    assert.equal(library.currentRelease, null);
});

const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {test} = require('node:test');
const vm = require('node:vm');

const template = readFileSync(__dirname + '/../templates/player/scripts.html', 'utf8');
const storesSource = template.slice(template.indexOf('    let playbackConnection'), template.indexOf("    Alpine.store('queue', {"));
const offline = readFileSync(__dirname + '/../templates/player/offline_scripts.html', 'utf8');
const applySource = offline.slice(offline.indexOf('        apply(mode)'), offline.indexOf('        async captureLibrary()'));

test('catalog requests stay on the device as soon as the browser disables networking', async () => {
    const location = new URL('https://example.test/');
    const navigator = {onLine:false};
    const store = {enabled:true, supported:true, active:false, automatic:false, localResponse: async () => Response.json(['saved'])};
    const requests = [];
    const window = {location, Alpine:{store:() => store}, fetch: async input => {
        requests.push(input);
        return new Response(null, {status:204});
    }};
    const fetchSource = template.slice(template.indexOf('const playerFetch ='), template.indexOf('const T ='));
    vm.runInNewContext(fetchSource, {location, navigator, window, URL, Request, Response});
    assert.deepEqual(await (await window.fetch('/api/player/playlists/-1')).json(), ['saved']);
    store.active = store.automatic = true;
    assert.equal((await window.fetch('/offline-probe')).status, 503);
    assert.deepEqual(requests, []);
    navigator.onLine = true;
    assert.equal((await window.fetch('/offline-probe')).status, 204);
    assert.deepEqual(requests, ['/offline-probe']);
});

function setup(fetch) {
    const stores = {offline: {active: true, saveState() {}, scheduleProbe() {}, played() {}, drain() {},
        isSaved: () => true, openAudio: async track => new Blob([String(track.id)])}};
    const created = [], revoked = [];
    const window = new EventTarget();
    const navigator = {onLine:true};
    const audio = Object.assign(new EventTarget(), {
        paused: true, ended: false, playCalls: 0, readyState: 0,
        hasAttribute(name) {return this[name] != null;},
        removeAttribute(name) {delete this[name];},
        load() {this.paused = true; this.readyState = 0; this.error = null;},
        async play() {
            ++this.playCalls;
            this.paused = false;
            this.dispatchEvent(new Event('play'));
            this.dispatchEvent(new Event('playing'));
        },
        pause() { this.paused = true; },
    });
    const document = Object.assign(new EventTarget(), {title: 'Player', documentElement: {classList: {toggle() {}}}});
    const storage = new Map([['furu_player_device_id', 'here']]);
    const sessionStorage = {
        getItem: key => storage.get(key), setItem: (key, value) => storage.set(key, value),
        removeItem: key => storage.delete(key),
    };
    vm.runInNewContext(storesSource + "\nObject.assign(Alpine.store('offline'), {" + applySource + "});", {
        GUEST: false, SHARE_TOKEN: 'skip-restore', Alpine: {effect() {}, store(name, value) { if (value) stores[name] = value; return stores[name]; }},
        audio, navigator, window, document, sessionStorage, AbortController, fetch, CustomEvent,
        retryTimer: 0, transfer: null, URL: {
            createObjectURL(file) {created.push(file); return 'blob:test/' + created.length;},
            revokeObjectURL(url) {revoked.push(url);},
        },
        playerOffline: () => stores.offline.active, OfflineCatalog: {trackId: t => t.id},
        setInterval: () => 0, setTimeout: () => 0, clearTimeout() {},
    });
    stores.player.init();
    stores.devices.init();
    return {stores, audio, created, revoked, navigator, mode: active => stores.offline.apply(active ? 'auto' : 'online')};
}

const settle = () => new Promise(resolve => setImmediate(resolve));

for (const monitoring of [true, false]) test('a former remote controller stays silent offline, Jam monitoring ' + monitoring, async () => {
    const {stores, audio, mode} = setup(() => assert.fail('Unexpected network request'));
    const {devices, player} = stores;
    mode(false);
    devices.activeDeviceId = 'other';
    devices.currentJamId = 'mixed';
    devices.jams = [{id: 'mixed', is_member: true, is_owner: false}];
    assert.equal(player._shouldSendRemote(), true);
    const saved = {id: 3, title: 'Song', stream_url: '/api/player/stream/3'};
    stores.queue = {tracks: [saved], syncCurrentIndexToTrack() {}};
    player.currentTrack = saved;
    player.currentTime = 12;
    player.isPlaying = true;
    player._localSourceTrackId = monitoring ? saved.id : 17;
    audio.paused = !monitoring;
    player._cachePlayCounted = true;
    mode(true);
    assert.equal(audio.paused, true, 'stop the monitor synchronously before reading IndexedDB');
    assert.equal(player.isPlaying, false);
    let nextCalls = 0;
    player.next = () => { nextCalls++; };
    audio.dispatchEvent(new Event('ended'));
    audio.dispatchEvent(new Event('playing'));
    assert.equal(nextCalls, 0);
    player.useOfflineCatalog([saved]);
    await settle();
    assert.equal(devices.isActive(), true);
    assert.equal(devices.selectedJam(), null);
    assert.equal(player._shouldSendRemote(), false);
    assert.equal(player.isPlaying, false);
    assert.equal(audio.playCalls, 0);
    assert.equal(player._cachePlayCounted, false);
    player.resume();
    assert.equal(audio.paused, false);
    assert.equal(audio.playCalls, 1);
    player.useOfflineCatalog([saved]); // A cache refresh must preserve explicit offline playback.
    assert.equal(audio.paused, false);
    assert.equal(audio.playCalls, 1);
    assert.equal(await devices.sendCommand('pause'), false);
});

test('only the online owner playing the displayed recording continues from cache', async () => {
    for (const [owner, paused, source, continues] of [
        ['here', false, 3, true], ['here', true, 3, false], ['here', false, 4, false], ['other', false, 3, false],
    ]) {
        const {stores, audio, mode} = setup(() => assert.fail('Unexpected network request'));
        mode(false);
        const saved = {id: 3, stream_url: '/api/player/stream/3'};
        stores.queue = {tracks: [saved], syncCurrentIndexToTrack() {}};
        stores.devices.activeDeviceId = owner;
        stores.player.currentTrack = saved;
        stores.player.isPlaying = !paused;
        stores.player._localSourceTrackId = source;
        audio.paused = paused;
        mode(true);
        stores.player.useOfflineCatalog([saved]);
        await settle();
        assert.equal(!audio.paused, continues);
        assert.equal(audio.playCalls, continues ? 1 : 0);
    }
});

test('cached online playback keeps the same blob and position when entering offline mode', async () => {
    const {stores, audio, created, revoked, mode} = setup(() => assert.fail('Unexpected network request'));
    const saved = {id:3, content_id:'recording', stream_url:'/api/player/stream/3'};
    const duplicate = {...saved, id:4};
    stores.queue = {tracks:[saved, duplicate], syncCurrentIndexToTrack() {}};
    mode(false);
    stores.offline.enabled = true;
    await stores.player._playLocal(saved);
    assert.equal(audio.src, 'blob:test/1');
    audio.currentTime = 9;
    mode(true);
    stores.player.useOfflineCatalog([saved, duplicate]);
    assert.equal(stores.player.currentTrack.id, 3);
    assert.deepEqual(stores.queue.tracks.map(track => track.id), [3,4]);
    assert.equal(audio.currentTime, 9);
    assert.equal(audio.playCalls, 1);
    assert.equal(created.length, 1);
    assert.deepEqual(revoked, []);
    stores.player.useOfflineCatalog([]);
    assert.equal(audio.src, undefined);
    assert.deepEqual(revoked, ['blob:test/1']);
});

test('a failed HTTP source resumes from cache only if it was playing when the network failed', async () => {
    for (const paused of [false, true]) {
        const {stores, audio, mode} = setup(() => assert.fail('Unexpected network request'));
        const saved = {id:3, stream_url:'/api/player/stream/3'};
        stores.queue = {tracks:[saved], syncCurrentIndexToTrack() {}};
        mode(false);
        await stores.player._playLocal(saved);
        audio.readyState = 4;
        audio.currentTime = 9;
        audio.dispatchEvent(new Event('timeupdate'));
        if (paused) stores.player.pause();
        audio.error = {code:2};
        audio.currentTime = 0;
        audio.paused = true;
        audio.dispatchEvent(new Event('pause'));
        audio.dispatchEvent(new Event('timeupdate'));
        mode(true);
        stores.player.useOfflineCatalog([saved]);
        await settle();
        audio.dispatchEvent(new Event('loadedmetadata'));
        assert.equal(audio.src, 'blob:test/1');
        assert.equal(audio.currentTime, 9);
        assert.equal(audio.paused, paused);
    }
});

test('only the latest file read can attach audio, and pause and seek apply while it opens', async () => {
    const {stores, audio, revoked} = setup(() => assert.fail('Unexpected network request'));
    const files = new Map();
    stores.offline.openAudio = track => new Promise(resolve => files.set(track.id, resolve));
    const track = id => ({id, stream_url:'/api/player/stream/'+id});
    const first = stores.player._playLocal(track(1));
    const second = stores.player._playLocal(track(2));
    stores.player.seek(12);
    stores.player.pause();
    files.get(2)(new Blob(['second']));
    await second;
    files.get(1)(new Blob(['first']));
    await first;
    audio.dispatchEvent(new Event('loadedmetadata'));
    assert.equal(audio.src, 'blob:test/1');
    assert.equal(audio.paused, true);
    assert.equal(audio.currentTime, 12);
    const third = stores.player._playLocal(track(3));
    files.get(3)(null);
    await third;
    assert.equal(audio.src, undefined, 'a missing offline file cannot fall through to HTTP');
    assert.deepEqual(revoked, ['blob:test/1']);
});

test('a native stream error switches to the file even before the browser announces offline', async () => {
    const {stores, audio, created, mode} = setup(() => assert.fail('Unexpected network request'));
    const track = {id:5, stream_url:'/api/player/stream/5'};
    stores.queue = {tracks:[track], syncCurrentIndexToTrack() {}};
    mode(false);
    await stores.player._playLocal(track);
    audio.readyState = 4;
    audio.currentTime = 12;
    audio.dispatchEvent(new Event('timeupdate'));
    // The download completed during HTTP playback. Firefox can fail the media request first.
    let finishRead;
    Object.assign(stores.offline, {
        enabled:true, networkResult() {}, openAudio:() => new Promise(resolve => {finishRead = resolve;}),
    });
    audio.paused = true;
    audio.error = {code:2};
    audio.dispatchEvent(new Event('error'));
    mode(true);
    stores.player.useOfflineCatalog([track]);
    finishRead(new Blob(['recording']));
    await settle();
    audio.dispatchEvent(new Event('loadedmetadata'));
    assert.equal(audio.src, 'blob:test/1');
    assert.equal(audio.currentTime, 12);
    assert.equal(audio.paused, false);
    assert.equal(created.length, 1);
});

test('late device polls and transfers cannot replace playback after going offline and reconnecting', async () => {
    const responses = [];
    const {stores, mode} = setup((url, {signal}) => new Promise(resolve => responses.push({
        url, signal, deliver: data => resolve({ok: true, json: async () => data}),
    })));
    const {devices, player} = stores;
    let remoteCommands = 0;
    player._executeRemoteCommand = () => { ++remoteCommands; };
    mode(false);
    const poll = devices.poll();
    const transfer = devices.select('here');
    assert.equal(responses.length, 2);
    mode(true);
    assert.ok(responses.every(r => r.signal.aborted));
    mode(false);
    const fresh = devices.heartbeat();
    responses[2].deliver({active_device_id: 'other', devices: [], jams: []});
    await fresh;
    for (const r of responses.slice(0, 2)) r.deliver({active_device_id: 'here', commands: [{command: 'play_track'}], playback_state: {}});
    await Promise.all([poll, transfer]);
    assert.equal(devices.activeDeviceId, 'other');
    assert.equal(devices.isActive(), false);
    assert.equal(remoteCommands, 0);
});

test('a poll finishing after browser disconnect cannot replace the playing file before the offline event', async () => {
    let requests = 0, deliverPoll;
    const {stores, audio, navigator, mode} = setup(() => {
        requests++;
        return {ok:true, json:() => new Promise(resolve => {deliverPoll = resolve;})};
    });
    const {player, devices, offline} = stores;
    mode(false);
    offline.enabled = true;
    const saved = {id:1, stream_url:'/api/player/stream/1'};
    const other = {id:2, stream_url:'/api/player/stream/2'};
    stores.queue = {tracks:[saved], syncCurrentIndexToTrack() {}, _tracksWithJamDefaults: tracks => tracks};
    devices.activeDeviceId = devices.id;
    await player._playLocal(saved);
    const poll = devices.poll();
    await settle();
    navigator.onLine = false;
    const payload = {track:other, tracks:[other], position_seconds:3};
    const command = {command:'play_track', payload};
    deliverPoll({active_device_id:'other', commands:[command], playback_state:payload});
    await poll;
    player._executeRemoteCommand(command);
    await devices.poll();
    await devices.heartbeat();
    assert.equal(requests, 1);
    assert.equal(devices.activeDeviceId, devices.id);
    assert.equal(player.currentTrack.id, saved.id);
    assert.equal(audio.paused, false);
    assert.equal(audio.src, 'blob:test/1');
    assert.equal(audio.playCalls, 1);
});

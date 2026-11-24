const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {test} = require('node:test');
const vm = require('node:vm');

const template = readFileSync(__dirname + '/../templates/player/scripts.html', 'utf8');
const storesSource = template.slice(template.indexOf('    let playbackConnection'), template.indexOf("    Alpine.store('queue', {"));
const offline = readFileSync(__dirname + '/../templates/player/offline_scripts.html', 'utf8');
const applySource = offline.slice(offline.indexOf('        apply(mode)'), offline.indexOf('        async captureLibrary()'));

function setup(fetch) {
    const stores = {offline: {active: true, saveState() {}, scheduleProbe() {}}};
    const window = new EventTarget();
    const audio = Object.assign(new EventTarget(), {
        paused: true, ended: false, playCalls: 0,
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
        audio, navigator: {}, window, document, sessionStorage, AbortController, fetch, CustomEvent,
        retryTimer: 0, transfer: null, playbackSource: t => t.stream_url,
        playerOffline: () => stores.offline.active, OfflineCatalog: {trackId: t => t.id},
        setInterval: () => 0, setTimeout: () => 0, clearTimeout() {},
    });
    stores.player.init();
    stores.devices.init();
    return {stores, audio, mode: active => stores.offline.apply(active ? 'auto' : 'online')};
}

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

test('only the online owner playing the displayed recording continues from cache', () => {
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
        assert.equal(!audio.paused, continues);
        assert.equal(audio.playCalls, continues ? 1 : 0);
    }
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

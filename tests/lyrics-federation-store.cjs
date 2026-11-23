const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {join} = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const script = readFileSync(join(__dirname, '../templates/player/lyrics_scripts.html'), 'utf8')
    .split('<script>')[1].split('</script>')[0];

function storeFor(fetch, guest = false) {
    let lyrics;
    vm.runInNewContext(script, {
        document: {
            addEventListener: (_, initialize) => initialize(),
            documentElement: {lang: 'en'},
        },
        Alpine: {
            store: (name, value) => name === 'lyrics' ? (lyrics = value) : {},
        },
        fetch, URLSearchParams, GUEST: guest, SHARE_TOKEN: 'test-share',
    });
    lyrics.visible = true;
    return lyrics;
}

const id = 'b3:' + 'a'.repeat(64);
const version = (id, plain_text) => ({id, plain_text, synced_lrc: null, instrumental: false});
const reply = (versions, content_id = id) => ({ok: true, json: async () => ({versions, content_id})});

test('saved lyrics display first and peer alternatives preserve the selection', async () => {
    let release;
    const peerReply = new Promise(resolve => { release = resolve; });
    let url;
    const lyrics = storeFor(endpoint => {
        url = endpoint;
        return endpoint.startsWith('/api/player/lyrics/') ?
            Promise.resolve(reply([version('local', 'Words')])) : peerReply;
    });
    const load = lyrics.load({id: '4'});
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(lyrics.currentVersion().id, 'local');
    assert.match(url, /\/federation\/tracks\/lyrics\?content_id=b3%3A/);
    release(reply([version('duplicate', 'Words'), version('peer', 'New words')]));
    await load;
    assert.equal(lyrics.versions.length, 2);
    assert.equal(lyrics.currentVersion().id, 'local');
});

test('guest lyrics use only the share endpoint', async () => {
    const urls = [];
    const lyrics = storeFor(async endpoint => {
        urls.push(endpoint);
        return reply([version('local', 'Words')]);
    }, true);
    await lyrics.load({id: 4});
    assert.deepEqual(urls, ['/api/player/shares/test-share/lyrics/4']);
});

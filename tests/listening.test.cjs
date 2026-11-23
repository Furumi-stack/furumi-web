const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {test} = require('node:test');
const vm = require('node:vm');
const script = readFileSync(__dirname + '/../templates/player/listening_scripts.html', 'utf8').replace(/<\/?script>/g, '');

function setup(fetch, stores = {}) {
    const Alpine = {store(name, state) {if (state) stores[name] = state; return stores[name];}};
    vm.runInNewContext(script, {Alpine, fetch, URLSearchParams, AbortController,
        document:{documentElement:{lang:'en'}, addEventListener(evt, fn) {fn();}}});
    return stores.listening;
}
const response = value => ({ok:true,json:async () => value});

test('a slow overview cannot replace a newer view, and history uses the selected calendar day', async () => {
    const requests = [];
    const listening = setup((url, options) => new Promise(resolve => requests.push({url,options,resolve})));
    const first = listening.enter('overview');
    const second = listening.enter('history','2026-10-25');
    assert.equal(requests[0].options.signal.aborted, true);
    const q = new URL(requests[1].url, 'https://player.test').searchParams;
    assert.equal(q.get('from'),'2026-10-25');
    assert.equal(q.get('to'),'2026-10-25');
    requests[1].resolve(response({items:[],page:1,limit:25,total:0}));
    await second;
    requests[0].resolve(response({ranking:[],daily:[],plays:0,tracks:0,listened_seconds:0}));
    await first;
    assert.equal(listening.overview, null);
    assert.equal(listening.loading, false);
    const third = listening.enter('history','2026-02-31');
    assert.equal(new URL(requests[2].url, 'https://player.test').searchParams.get('from'), 'all');
    requests[2].resolve(response({items:[],page:1,limit:25,total:0}));
    await third;
});

test('leaving and reentering during a track lookup never replaces the queue later', async () => {
    const requests = [];
    const library = {view:'listening'};
    const plays = [];
    const queue = {playRelease(...args) {plays.push(args);}};
    const listening = setup((url) => new Promise(resolve => requests.push({url,resolve})), {library, queue});
    listening.tab = 'history';
    const row = {id:'after', track_id:2, content_id:'content'};
    listening.history = {items:[row]};
    const click = listening.play(row);
    library.view = 'artists';
    library.view = 'listening';
    const visit = listening.enter('overview');
    requests[0].resolve(response([{id:2}]));
    await click;
    requests[1].resolve(response({ranking:[],daily:[],plays:0,tracks:0,listened_seconds:0}));
    await visit;
    assert.equal(plays.length, 0);
    assert.equal(listening.actionError, '');
    assert.equal(listening.tab, 'overview');
});

test('the chart fills quiet calendar days including the leap day', () => {
    const listening = setup(() => assert.fail('Unexpected request'));
    const points = listening.dailyPoints([{day:'2024-03-01',listened_seconds:60,plays:1}],
        {from:'2024-02-28',to:'2024-03-01'});
    assert.deepEqual(Array.from(points, row => [row.day,row.height]), [
        ['2024-02-28',0], ['2024-02-29',0], ['2024-03-01',100],
    ]);
});

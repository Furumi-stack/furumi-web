const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {test} = require('node:test');
const vm = require('node:vm');
const script = readFileSync(__dirname + '/../templates/player/listening_scripts.html', 'utf8').replace(/<\/?script>/g, '');

function setup(fetch, stores = {}, localStorage) {
    const Alpine = {store(name, state) {if (state) stores[name] = state; return stores[name];}};
    vm.runInNewContext(script, {Alpine, fetch, URLSearchParams, AbortController, localStorage,
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

test('the listening calendar aligns Monday weeks across leap day and month boundaries', () => {
    const listening = setup(() => assert.fail('Unexpected request'));
    const points = listening.dailyPoints([], {from:'2024-02-29',to:'2024-04-02'});
    const calendar = listening.buildHeatmap(points);
    const position = day => {
        const cell = calendar.days.find(point => point.day === day);
        return [cell.week, cell.weekday];
    };
    assert.equal(calendar.days.length, 34);
    assert.equal(calendar.weeks, 6);
    assert.deepEqual(position('2024-02-29'), [0,3]);
    assert.deepEqual(position('2024-03-01'), [0,4]);
    assert.deepEqual(position('2024-03-31'), [4,6]);
    assert.deepEqual(position('2024-04-01'), [5,0]);
    assert.equal(calendar.weekdays.length, 7);
    assert.ok(calendar.months.some(month => month.week === 5 && month.label.startsWith('Apr')));
    assert.ok(calendar.days.every(cell => cell.level === 0));
});

test('calendar intensity follows play count even when longer sessions have fewer plays', () => {
    const listening = setup(() => assert.fail('Unexpected request'));
    const rows = [
        {day:'2024-04-01', plays:1, listened_seconds:10000},
        {day:'2024-04-02', plays:2, listened_seconds:9000},
        {day:'2024-04-03', plays:4, listened_seconds:4000},
        {day:'2024-04-04', plays:8, listened_seconds:1500},
        {day:'2024-04-05', plays:16, listened_seconds:600},
        {day:'2024-04-06', plays:1, listened_seconds:10},
    ];
    const points = listening.dailyPoints(rows, {from:'2024-04-01',to:'2024-04-07'});
    const {days} = listening.buildHeatmap(points);
    const levels = Array.from(days, cell => cell.level);
    assert.ok(levels.every(level => Number.isInteger(level) && level >= 0 && level <= 4));
    assert.deepEqual(levels.slice(0,5), levels.slice(0,5).sort((a,b) => a-b));
    assert.ok(levels[0] > 0 && levels[0] < levels[4]);
    assert.equal(levels[4], 4);
    assert.equal(levels[0], levels[5]);
    assert.equal(levels[6], 0);
});

test('an invalid or inaccessible chart preference falls back to bars and can still be switched', () => {
    const noRequest = () => assert.fail('Changing the chart should not request new listening data');
    for (const storage of [
        {getItem() { return 'unknown'; }, setItem() {}},
        {getItem() { throw new Error('Storage blocked'); }, setItem() { throw new Error('Storage blocked'); }},
    ]) {
        const listening = setup(noRequest, {}, storage);
        assert.equal(listening.chartMode, 'bars');
        listening.setChartMode('heatmap');
        assert.equal(listening.chartMode, 'heatmap');
    }
});

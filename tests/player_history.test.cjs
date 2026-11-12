const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {test} = require('node:test');
const vm = require('node:vm');
const template = readFileSync(`${__dirname}/../templates/player/scripts.html`, 'utf8');
function player(fetchImpl = async () => ({ok:true}), beacon = () => true) {
    // Exercise the production methods with audio and network stubs.
    const methods = template.slice(template.indexOf('        _recordHistory(endedReason'), template.indexOf('        _sendNowPlaying()'));
    const audio = {currentTime:60};
    const context = vm.createContext({audio, fetch:fetchImpl, navigator:{sendBeacon:beacon}, Blob,
        globalThis:{crypto:{randomUUID:()=> 'uuid'}}, setTimeout: fn => { fn(); }, console});
    vm.runInContext('this.methods = {' + methods + '};', context);
    return Object.assign(context.methods, {
        currentTrack:{id:8,duration_seconds:180}, _historyTrack:{id:8,duration_seconds:180},
        _localSourceTrackId:8, _listenId:'event1', _playbackStartedAt:123456,
        _historyRecorded:false, _listenedSeconds:60, _lastAudioTime:60, isPlaying:true,
    });
}
const flush = () => new Promise(resolve => setImmediate(resolve));
test('switching tracks records short sessions, and completion is not counted twice', async () => {
    const calls=[]; const p=player(async (_url, options)=> {calls.push(JSON.parse(options.body)); return {ok:true};});
    p._listenedSeconds=8; p._recordHistory('replaced'); await flush();
    assert.equal(calls[0].duration_listened,8); assert.equal(calls[0].ended_reason,'replaced');
    p._recordHistory('skipped'); await flush(); assert.equal(calls.length,1);
});
test('natural finish records final duration; seeking does not create listened time', async () => {
    const calls=[]; const p=player(async (_url, options)=> {calls.push(JSON.parse(options.body)); return {ok:true};});
    p._listenedSeconds=179.4; p._recordHistory('finished'); await flush();
    assert.equal(calls[0].duration_listened,180); assert.equal(calls[0].completed,true);
    p._listenedSeconds=0; assert.equal(p._historyListenedSeconds(false),0);
});
test('failed submission retries the original event even after track changes', async () => {
    const calls=[]; const p=player(async (_url,options)=> {calls.push(JSON.parse(options.body)); return {ok:calls.length>1,status:503};});
    p._recordHistory('skipped'); p.currentTrack={id:9}; p._listenId='event2';
    await flush(); assert.equal(calls.length,2); assert.deepEqual(calls[0], calls[1]);
    assert.equal(calls[0].track_id,8);
});
test('page exit uses beacon and does not duplicate a recorded event', () => {
    const calls=[]; const p=player(()=>assert.fail('unexpected fetch'), (url,blob)=>{calls.push({url,blob});return true;});
    p._recordHistory('stopped',true); p._recordHistory('stopped',true);
    assert.equal(calls.length,1); assert.equal(calls[0].url,'/api/player/history');
});
test('remote UI updates cannot attribute local audio to the wrong track', async () => {
    let body; const p=player(async (_url,options)=> {body=JSON.parse(options.body);return {ok:true}});
    p.currentTrack={id:999}; p._recordHistory('replaced'); await flush(); assert.equal(body.track_id,8);
});
test('unstarted tracks and sub-five-second interrupted sessions are not recorded', async () => {
    const p=player(()=>assert.fail('unexpected fetch'));
    p._playbackStartedAt=null; p._recordHistory('finished');
    p._playbackStartedAt=123; p._listenedSeconds=4; p._recordHistory('skipped'); await flush();
    assert.equal(p._historyRecorded,false);
});

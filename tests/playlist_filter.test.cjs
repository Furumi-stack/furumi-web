const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const {test} = require('node:test');
const script = fs.readFileSync(__dirname + '/../templates/player/playlist_filter_scripts.html','utf8').replace(/<\/?script>/g,'');

function setup(reduce = false) {
    let component, settle;
    const window = {innerHeight:800,matchMedia:()=>({matches:reduce})};
    vm.runInNewContext(script,{
        document:{addEventListener:(_,fn)=>fn()}, Alpine:{data:(_,make)=>{ component=make(); }}, window,
        setTimeout:fn=>{ settle=fn; return 1; }, clearTimeout:()=>{ settle=null; },
    });
    return {component, finish:()=>{if (settle) settle();}};
}
const visibleIds = component => Array.from(component.visibleRows.filter(r=>!r.leaving),r=>r.track.id);

test('playlist filters primary and featured credits, preserving original positions and duplicate identities', () => {
    const {component} = setup(true);
    const track = {id:3,title:'Ёжик',artists:[{name:'Primary'}],featured_artists:[{name:'Гость'}]};
    const playlist = {id:10,kind:'user',tracks:[{id:1,title:'One'},track,track]};
    component.setPlaylist(playlist);
    component.setQuery('  ГОСТЬ   ёжик ');
    assert.deepEqual(visibleIds(component),[3,3]);
    assert.deepEqual(Array.from(component.visibleRows,r=>r.idx),[1,2]);
    assert.equal(new Set(component.visibleRows.map(r=>r.key)).size,2);
    const late = {...track,id:'fed:remote',content_id:'remote'};
    component.setPlaylist({...playlist,tracks:[...playlist.tracks,late]});
    assert.deepEqual(visibleIds(component),[3,3,'fed:remote']);
    component.setPlaylist({...playlist,id:11});
    assert.equal(component.query,'');
    assert.equal(component.matchCount,3);
});

test('large playlist transitions retain only viewport neighbours and survive rapid reversals', () => {
    const {component,finish} = setup();
    const tracks = Array.from({length:1000},(_,id)=>({id,title:id === 999 ? 'Needle':'Song'}));
    component.setPlaylist({id:-1,kind:'likes',tracks});
    component.setQuery('needle');
    assert.deepEqual(visibleIds(component),[999]);
    assert.ok(component.visibleRows.some(r=>r.leaving));
    assert.ok(component.visibleRows.length < 40,'offscreen rows are removed immediately');
    component.setQuery('');
    assert.equal(component.matchCount,1000);
    assert.ok(component.visibleRows.filter(r=>r.entering).length < 40);
    finish();
    assert.equal(component.visibleRows.length,1000);
    component.setQuery('missing');
    finish();
    assert.equal(component.visibleRows.length,0);
    component.setQuery('');
    assert.equal(component.visibleRows[0].entering,true);
    component.destroy();
});

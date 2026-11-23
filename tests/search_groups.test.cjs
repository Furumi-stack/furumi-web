const assert=require('node:assert/strict'),{test}=require('node:test'),fs=require('node:fs'),vm=require('node:vm');
const script=fs.readFileSync(`${__dirname}/../templates/player/search_group_scripts.html`,'utf8').replace(/<\/?script>/g,'');
function setup(){const stores={queue:{playRelease(tracks,index){this.tracks=tracks;this.index=index;}}};const context=vm.createContext({clearTimeout,setTimeout,window:{matchMedia:()=>({matches:false})},document:{documentElement:{lang:'en'},addEventListener(_,fn){fn();}},Alpine:{store(name,value){if(value)stores[name]=value;return stores[name];}}});vm.runInContext(script,context);return {groups:stores.searchGroups,stores,context};}
const local=(id,title='Still Loving You',artist='Scorpions')=>({id,title,artists:[{id:1,name:artist}],release_title:'Album '+id});
const ids=rows=>Array.from(rows,row=>row.id);
test('groups nonadjacent titles by performer, preserving original indices and versions',()=>{
 const {groups}=setup(),tracks=[local(1),local(2,'Other'),local(3,' STILL  LOVING YOU '),local(4,'Still Loving You','Someone else'),local(5,'Still Loving You (Live)')];
 const rows=groups.rows(tracks,'local');assert.deepEqual(ids(rows),[1,2,4,5]);assert.equal(rows[0]._searchGroupCount,2);
 assert.ok(rows.slice(1).every(row=>row._searchGroupCount===1));assert.equal(tracks[0]._searchGroupKey,undefined);
 groups.toggle(rows[0]);const expanded=groups.rows(tracks,'local');
 assert.deepEqual(ids(expanded),[1,3,2,4,5]);assert.equal(expanded[1]._searchSourceIndex,2);
});
test('expansion survives streamed federation updates and resets for a new search',()=>{
 const {groups}=setup();const remote=id=>({key:{content_id:'b3:'+id},metadata:{title:'Song',artists:[{name:'Artist'}]},availability:{federation:[{owner:'peer',item_id:id}]}});
 let rows=groups.rows([remote(1),remote(2)],'federation');groups.toggle(rows[0]);
 rows=groups.rows([remote(1),remote(2),remote(3)],'federation');assert.equal(rows[0]._searchGroupCount,3);assert.ok(rows.every(row=>!row._searchGroupHidden));
 assert.equal(new Set(rows.map(row=>row._searchRowId)).size,3);assert.equal(rows[0]._searchGroupControls.split(' ').length,2);
 assert.ok(rows.slice(1).every(row=>row._searchGroupControls===null));
 groups.reset();assert.equal(groups.rows([remote(1),remote(2)],'federation').length,1);
});
test('audio similarity keeps its original ungrouped ranking',()=>{
 const {groups}=setup(),tracks=[local(1),local(2,'Other'),local(3)];const rows=groups.rows(tracks,'local',false);
 assert.deepEqual(ids(rows),[1,2,3]);assert.ok(rows.every(row=>row._searchGroupCount===1&&!row._searchGroupHidden));
});
test('playing a grouped search uses the selected source identity and only visible versions',()=>{
 const {groups,stores,context}=setup(),template=fs.readFileSync(`${__dirname}/../templates/player/scripts.html`,'utf8');
 const method=template.slice(template.indexOf('        playSearchTrack(idx)'),template.indexOf('        _setupScroll()',template.indexOf('        playSearchTrack(idx)')));
 vm.runInContext('this.library={'+method+'};',context);const library=context.library;library.displaySearchResults={tracks:[local(1),local(2,'Other'),{...local('fed:3'),federation_only:true}]};
 library.playSearchTrack(0);assert.deepEqual(ids(stores.queue.tracks),[1,2]);assert.equal(stores.queue.index,0);
 library.playSearchTrack(1);assert.equal(stores.queue.tracks[stores.queue.index].id,2);
 groups.toggle(groups.rows(library.displaySearchResults.tracks,'search')[0]);library.playSearchTrack(2);
 assert.deepEqual(ids(stores.queue.tracks),[1,'fed:3',2]);assert.equal(stores.queue.tracks[stores.queue.index].id,'fed:3');
});

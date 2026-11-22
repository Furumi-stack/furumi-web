const assert=require('node:assert/strict');
const {test}=require('node:test');
const fs=require('node:fs'),vm=require('node:vm');
const source=fs.readFileSync(`${__dirname}/../templates/player/lyrics_scripts.html`,'utf8').replace(/<\/?script>/g,'');
function setup(fetchImpl=async()=>({ok:true,json:async()=>({versions:[]})})) {
    const stores={player:{currentTrack:{id:1,title:'One'},currentTime:0,seek(time){this.currentTime=time;}},info:{close(){}}};
    const document={documentElement:{lang:'en'},body:{style:{overflow:''}},activeElement:{focus(){}},addEventListener(_,fn){fn();},querySelector(){return null;}};
    vm.runInNewContext(source,{document,Alpine:{store(name,value){if(value)stores[name]=value;return stores[name];}},GUEST:false,SHARE_TOKEN:'',fetch:fetchImpl,Date,requestAnimationFrame:fn=>fn(),window:{matchMedia:()=>({matches:true})}});
    return {store:stores.lyrics,stores,document};
}
const plain={id:'plain',label:'Plain',plain_text:'Plain words',synced_lrc:null};
const timed={id:'timed',label:'Timed',plain_text:'Timed words',synced_lrc:'[00:02.00]First\n[00:05.50]Second'};
const settle=()=>new Promise(resolve=>setImmediate(resolve));
test('LRC parses repeated timestamps, offsets and malformed tags',()=>{
    const {store}=setup();
    assert.deepEqual(JSON.parse(JSON.stringify(store.parse('[ar:Artist]\n[offset:-500]\n[00:02.50][01:03] words\n[00:00.1]start\n[00:60]bad'))),[{ms:0,text:'start'},{ms:2000,text:'words'},{ms:62500,text:'words'}]);
    assert.equal(store.parse('plain\n[00:NaN]bad').length,0);
});
test('timed version is selected first; playback and backward seeks select the correct line',async()=>{
    const {store,stores}=setup(async()=>({ok:true,json:async()=>({versions:[plain,timed]})}));
    store.open(stores.player.currentTrack,true);await settle();
    assert.equal(store.currentVersion().id,'timed');
    store.tick(stores.player.currentTrack,1);assert.equal(store.active,-1);
    store.tick(stores.player.currentTrack,6);assert.equal(store.active,1);
    store.tick(stores.player.currentTrack,3);assert.equal(store.active,0);
    store.seek(store.lines[1]);assert.equal(stores.player.currentTime,5.5);
    store.choose(1);assert.equal(store.plain(),'Plain words');assert.equal(store.lines.length,0);
});
test('late responses cannot overwrite a new track; fullscreen stays open across misses',async()=>{
    let resolveFirst;
    const first=new Promise(resolve=>resolveFirst=resolve);
    const {store,stores}=setup(async url=>url.endsWith('/1')?first:{ok:true,json:async()=>({versions:[]})});
    store.open(stores.player.currentTrack,true);
    stores.player.currentTrack={id:2,title:'Two'};
    store.tick(stores.player.currentTrack,0);await settle();
    resolveFirst({ok:true,json:async()=>({versions:[timed]})});await settle();
    assert.equal(store.track.id,2);assert.equal(store.versions.length,0);assert.equal(store.visible,true);
    stores.player.currentTrack={id:3,title:'Three'};store.tick(stores.player.currentTrack,0);await settle();
    assert.equal(store.track.id,3);
});
test('preview does not follow playback or seek a different track; close restores scrolling',async()=>{
    const {store,stores,document}=setup(async()=>({ok:true,json:async()=>({versions:[timed]})}));
    store.open({id:9,title:'Preview'},false);await settle();
    store.tick(stores.player.currentTrack,10);assert.equal(store.track.id,9);assert.equal(store.active,-1);
    store.seek(store.lines[0]);assert.equal(stores.player.currentTime,0);
    assert.equal(document.body.style.overflow,'hidden');store.close();assert.equal(document.body.style.overflow,'');
});
test('errors remain retryable and podcast transitions clear previous lyrics',async()=>{
    let fail=true;
    const {store,stores}=setup(async()=>{if(fail)throw Error('offline');return {ok:true,json:async()=>({versions:[timed]})};});
    store.open(stores.player.currentTrack,true);await settle();assert.equal(store.error,true);
    fail=false;await store.load(stores.player.currentTrack);assert.equal(store.error,false);assert.equal(store.lines.length,2);
    stores.player.currentTrack={id:'ep1:one',podcast:{}};store.tick(stores.player.currentTrack,0);
    assert.equal(store.versions.length,0);assert.equal(store.visible,true);
});

const {test}=require('node:test'),assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm');
const scripts=fs.readFileSync(`${__dirname}/../templates/player/scripts.html`,'utf8');
function setup(fetchImpl){
 const stores={lyrics:{open(...args){this.opened=args;}},searchGroups:{reset(){}}};let timer,delay;
 const context={document:{documentElement:{lang:'ru'},addEventListener(_,fn){fn();}},Alpine:{store(name,value){if(value)stores[name]=value;return stores[name];}},GUEST:false,AbortController,fetch:fetchImpl,setTimeout(fn,ms){timer=fn;delay=ms;return 1;},clearTimeout(){timer=null;}};
 vm.runInNewContext(fs.readFileSync(`${__dirname}/../templates/player/lyrics_search_scripts.html`,'utf8').replace(/<\/?script>/g,''),context);
 const fields=scripts.slice(scripts.indexOf("        searchResults: null,"),scripts.indexOf("        searchLoading: false,",scripts.indexOf("        searchResults: null,")));
 const search=scripts.slice(scripts.indexOf('        async search(query, options = {}) {'),scripts.indexOf('        async searchSimilar('));
 const store=vm.runInNewContext('({'+fields+search+'})',context);
 Object.assign(store,{view:'artists',searchQuery:'snow',searchLoading:false,stopFederationSearch(){this.stopped=true;},startFederationSearch(){this.federationStarted=true;},_beginNavigation(hash){this.cancelPendingSearch();this.hash=hash;},_afterNavigation(){},applyFederationArtworkFallbacks(){},clearSearch(){this.cancelPendingSearch();this.searchQuery='';this.searchResults=null;}});
 return {store,stores,runTimer:()=>timer?.(),delay:()=>delay};
}
test('mode-specific debounce and Enter cancel earlier scheduled searches',async()=>{
 let calls=0;const {store,delay,runTimer}=setup(async()=>{calls++;return {ok:true,json:async()=>({tracks:[]})};});
 store.queueSearch();assert.equal(delay(),300);store.toggleSearchMode();assert.equal(delay(),1800);assert.equal(store.searchMode,'lyrics');
 await store.search(store.searchQuery);runTimer();assert.equal(calls,1);assert.equal(store.federationStarted,undefined);assert.match(store.hash,/mode=lyrics/);
});
test('late text searches cannot replace newer results or restore loading after a mode change',async()=>{
 const pending=[];const {store}=setup(()=>new Promise(resolve=>pending.push(resolve)));
 store.searchMode='lyrics';const first=store.search('first');const second=store.search('second');
 pending[1]({ok:true,json:async()=>({tracks:[{id:2}]})});await second;
 pending[0]({ok:true,json:async()=>({tracks:[{id:1}]})});await first;assert.equal(store.searchResults.tracks[0].id,2);
 const third=store.search('third');store.toggleSearchMode();pending[2]({ok:true,json:async()=>({tracks:[{id:3}]})});await third;assert.equal(store.searchResults,null);assert.equal(store.searchLoading,false);
});

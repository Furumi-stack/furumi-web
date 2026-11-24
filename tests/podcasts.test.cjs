const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {test} = require('node:test');
const vm = require('node:vm');
const source = readFileSync(`${__dirname}/../templates/player/podcast_scripts.html`, 'utf8').replace(/<\/?script>/g, '');
function setup(responder=()=>({})) {
    const calls=[],stores={library:{view:'artists',_beginNavigation(){},_afterNavigation(){}},mobile:{closeLibrary(){}},player:{_playLocal(track,options){this.currentTrack=track;this.options=options;},seek(){}}};
    vm.runInNewContext(source,{
        document:{documentElement:{lang:'ru'},addEventListener(_,fn){fn();}},
        Alpine:{store(name,value){if(value)stores[name]=value;return stores[name];}},
        fetch:async(url,options)=>{const body=JSON.parse(options.body);calls.push(body);return {ok:true,json:async()=>responder(body)};},
        confirm:()=>true,setInterval:()=>1,Date,Map,Set,Blob,location:{hash:''},navigator:{sendBeacon(){}},
    });
    return {store:stores.podcasts,stores,calls};
}
const episode={id:'ep1:fixture',position_ms:123000,completed:false,metadata:{podcast_id:'pod1:fixture',title:'Episode',duration_ms:3600000,season:2}};

test('podcast playback resumes through the main player with episode identity and its own progress generation',async()=>{
    const {store,stores,calls}=setup(()=>({generation:7}));
    store.selected={metadata:{title:'Show'}};
    await store.play({...episode});
    assert.equal(stores.player.currentTrack.id,episode.id);
    assert.equal(stores.player.currentTrack.podcast,true);
    assert.equal(stores.player.currentTrack.podcast_generation,7);
    assert.equal(stores.player.currentTrack.artists[0].name,'Show');
    assert.equal(stores.player.options.position_seconds,123);
    assert.equal(calls[0].explicit,true);
    stores.player.currentTime=130;
    store.checkpoint(stores.player);
    assert.equal(calls[1].position_ms,130000);
    assert.equal(calls[1].generation,7);
});
test('a completed episode starts at zero and restart resets a partial episode',async()=>{
    const {store,stores}=setup(()=>({generation:2}));store.selected={metadata:{title:'Show'}};
    await store.play({...episode,completed:true});assert.equal(stores.player.options.position_seconds,0);
    const item={...episode};await store.restart(item);assert.equal(item.position_ms,0);assert.equal(stores.player.options.position_seconds,0);
});
test('marking unplayed is an explicit reset and advances the active session generation',async()=>{
    const {store,stores,calls}=setup(()=>({generation:9}));
    const item={...episode,completed:true};stores.player.currentTrack={id:item.id,podcast:true};
    await store.mark(item);assert.equal(item.completed,false);assert.equal(item.position_ms,0);
    assert.equal(calls[0].explicit,true);assert.equal(calls[0].position_ms,0);assert.equal(stores.player.currentTrack.podcast_generation,9);
});
test('filtering combines season, title and played status without mixing podcasts with music',()=>{
    const {store}=setup();store.episodes=[{...episode},{...episode,id:'other',completed:true},{...episode,id:'season3',metadata:{...episode.metadata,season:3}}];
    store.filter='epis';store.season='2';store.unplayedOnly=true;assert.deepEqual(Array.from(store.filteredEpisodes(),e=>e.id),[episode.id]);
    store.public=[{id:'a',url:'origin'}];store.hits=[{id:'a',url:'origin'},{id:'b',url:'other'}];store.searched=true;assert.equal(store.publicCards().length,2);
});
test('private URL publication requires a separate confirmation; revocation does not republish',async()=>{
    const {store,calls}=setup(body=>body.action==='share'&&!body.confirmed?{confirmation_required:true,urls:['https://example.test/feed?token=private']}:body.action==='list'?{mine:[],public:[]}:{ok:true});
    await store.share({id:'a',shared:false,metadata:{feed_url:'https://example.test/feed?token=private'}});
    // UI actions return task promises through the busy flag; flush the asynchronous load.
    while(store.busy)await new Promise(resolve=>setImmediate(resolve));
    const shares=calls.filter(c=>c.action==='share');assert.equal(shares.length,2);assert.equal(shares[1].confirmed,true);
});
test('admin and player inline scripts parse without a browser',()=>{
    for(const file of ['templates/admin/podcasts.html','templates/player/podcast_scripts.html']) {
        const html=readFileSync(`${__dirname}/../${file}`,'utf8');for(const match of html.matchAll(/<script>([\s\S]*?)<\/script>/g))new vm.Script(match[1]);
    }
});

test('new episodes do not create gaps in previously loaded pages',async()=>{
    let items=Array.from({length:202},(_,i)=>({id:'episode-'+i,metadata:{title:String(i)}}));
    const {store}=setup(body=>({episodes:items.slice(body.offset,body.offset+200),has_more:body.offset+200<items.length}));
    store.selected={id:'show'};
    await store.loadEpisodes();await store.loadEpisodes(true);assert.equal(store.episodes.length,202);
    items=[{id:'new',metadata:{title:'new'}},...items];
    await store.loadEpisodes();assert.equal(store.episodes.length,203);
    assert.equal(store.episodes[0].id,'new');assert.ok(store.episodes.some(e=>e.id==='episode-199'));
});

test('opening podcast navigation starts polling and search results open existing subscriptions',async()=>{
    const podcast={id:'show',metadata:{title:'Show'},subscribed:true};
    const {store,stores,calls}=setup(body=>body.action==='list'?{mine:[podcast],public:[]}:body.action==='episodes'?{episodes:[],has_more:false}:{});
    await store.enter();assert.equal(store.poller,1);assert.equal(stores.library.view,'podcasts');
    stores.library.view='search';await store.openResult(podcast);
    assert.equal(stores.library.view,'podcast_detail');assert.equal(store.selected.id,'show');
    assert.equal(calls.filter(c=>c.action==='add').length,0);
});

test('podcast search displays matching results instead of retaining unrelated public cards',async()=>{
    const {store}=setup(()=>({hits:[{id:'matching',metadata:{title:'Found'}}]}));
    store.public=[{id:'unrelated'}];store.query='космос';await store.search();
    assert.deepEqual(Array.from(store.publicCards(),p=>p.id),['matching']);
    store.resetSearch();assert.deepEqual(Array.from(store.publicCards(),p=>p.id),['unrelated']);
});

test('public podcast cards preview details without subscribing; only the subscribe button adds the feed',async()=>{
    const pod={id:'public',url:'https://example.test/feed',metadata:{title:'Public',image_url:'https://example.test/cover.jpg'}};
    let subscribed=false;
    const {store,stores,calls}=setup(body=>{
        if(body.action==='add'){subscribed=true;return {id:pod.id};}
        if(body.action==='list')return {mine:subscribed?[pod]:[],public:[pod]};
        return {episodes:[],has_more:false};
    });
    await store.open(pod);assert.equal(stores.library.view,'podcast_detail');
    assert.equal(store.selected.subscribed,false);assert.equal(calls.filter(c=>c.action==='add').length,0);
    assert.equal(calls.filter(c=>c.action==='episodes').length,0);
    await store.subscribe();assert.equal(store.selected.subscribed,true);
    assert.equal(calls.filter(c=>c.action==='add').length,1);assert.equal(calls.filter(c=>c.action==='episodes').length,1);
    assert.equal(store.artwork(pod),pod.metadata.image_url);store.artworkFailed(pod);assert.equal(store.artwork(pod),null);
});

test('back navigation wins over a slow detail request',async()=>{
    let finish;
    const {store,stores}=setup(body=>body.action==='detail'?new Promise(resolve=>{finish=resolve;}):{mine:[],public:[]});
    const opened=store.open('slow');while(!finish)await new Promise(resolve=>setImmediate(resolve));
    await store.enter();finish({id:'slow',metadata:{title:'Old'}});await opened;
    assert.equal(stores.library.view,'podcasts');assert.equal(store.selected,null);assert.equal(store.loading,false);
});

test('hash routing restores both the podcast list and a selected podcast',()=>{
    const script=readFileSync(`${__dirname}/../templates/player/scripts.html`,'utf8');
    const start=script.indexOf('        _navigateFromHash(options = {}) {');const end=script.indexOf('        _scrollElement()',start);
    const calls=[],location={hash:'#podcasts'};
    const context=vm.createContext({GUEST:false,playerOffline:()=>false,location,URLSearchParams,Alpine:{store(){return {enter(options){calls.push(['list',options]);},open(id,options){calls.push(['detail',id,options]);}};}}});
    const library=vm.runInContext('({'+script.slice(start,end)+'})',context);library.goArtists=()=>{throw Error('unexpected artist navigation');};
    library._navigateFromHash({fromHash:true});assert.equal(calls[0][0],'list');assert.equal(calls[0][1].fromHash,true);
    location.hash='#podcast?id='+encodeURIComponent('pod1:fixture');library._navigateFromHash({fromHash:true});
    assert.equal(calls[1][0],'detail');assert.equal(calls[1][1],'pod1:fixture');
});

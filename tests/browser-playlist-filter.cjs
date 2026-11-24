// Render the actual player with FURU_PODCAST_RENDER_FIXTURE=1 before running.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const html = fs.readFileSync('target/podcast-player-fixture.html','utf8');
const guestHtml = fs.readFileSync('target/shared-player-fixture.html','utf8');
const makeTrack = (id,title,artist,featured=[]) => ({
    id,title,artists:[{id:3,name:artist}],featured_artists:featured.map(name=>({id:4,name})),
    content_id:'b3:'+String(id).padStart(64,'0'),sort_key:50-id,release_id:9,
    release_title:'Обычный альбом',duration_seconds:200,stream_url:'/api/player/stream/'+id,
});
const songs = [makeTrack(11,'Июльское небо','Лето',['Гость Весна']),
    makeTrack(12,'Снег на рассвете','Север'),makeTrack(13,'Ночной вокзал','Звезда')];
const remote = {position:0, federation:{content_id:'b3:'+'f'.repeat(64),title:'Лунный свет',artist_names:['Артист сети'],
    owner:'peer',item_id:'peer-song',duration_seconds:180}};
const owned = {id:7,title:'Мой плейлист',kind:'user',is_own:true,
    tracks:[...songs,songs[1]].map((t,i)=>({...t,playlist_track_id:201+i}))};
const published = {...owned,id:8,title:'Опубликованный плейлист',is_own:false,is_public:true,tracks:songs};
const likes = {id:-1,title:'Likes',kind:'likes',is_own:true,tracks:songs};

async function sampleTransition(input, query, index) {
    return input.evaluate(async (el, {query,index}) => {
        el.value = query;
        el.dispatchEvent(new Event('input',{bubbles:true}));
        const selector = '.playlist-filter-row:has([data-playlist-index="' + index + '"])';
        const frame = () => new Promise(resolve=>requestAnimationFrame(resolve));
        const until = performance.now()+500;
        while (performance.now()<until) {
            const row = document.querySelector(selector);
            const effects = row?.getAnimations().filter(a => a.effect.getTiming().duration > 0) || [];
            if (effects.length) {
                const measures = [];
                for (const fraction of [0,.5,1]) {
                    for (const animation of effects) {
                        animation.pause();
                        animation.currentTime = animation.effect.getTiming().duration*fraction;
                    }
                    measures.push([row.getBoundingClientRect().height,Number(getComputedStyle(row).opacity)]);
                }
                const duration = Math.max(...effects.map(a=>a.effect.getTiming().duration));
                for (const animation of effects) animation.play();
                return {duration,measures};
            }
            await frame();
        }
        throw Error('The filtered row did not animate');
    }, {query,index});
}

async function check(browser,width) {
    const mobile = width<600;
    const page = await browser.newPage({viewport:{width,height:844},hasTouch:mobile,isMobile:mobile,reducedMotion:'reduce'});
    page.setDefaultTimeout(5000);
    const errors=[], searchRequests=[];
    page.on('pageerror',err=>errors.push(err.message));
    await page.route('**/*',route=>{
        const {pathname} = new URL(route.request().url());
        if (pathname==='/') return route.fulfill({contentType:'text/html',body:html});
        if (pathname==='/share/playlist-filter-test') return route.fulfill({contentType:'text/html',body:guestHtml});
        if (pathname==='/api/player/shares/playlist-filter-test') {
            return route.fulfill({json:{title:'Shared tracks',tracks:songs.map(t=>({...t,
                stream_url:'/api/player/shares/playlist-filter-test/stream/'+t.id}))}});
        }
        if (pathname.includes('/search')) searchRequests.push(pathname);
        if (pathname.endsWith('/federation-tracks')) return route.fulfill({json:pathname.includes('/-1/')?[remote]:[]});
        const replies = {
            '/api/player/playlists/-1':likes, '/api/player/playlists/7':owned,'/api/player/playlists/8':published,
            '/api/player/playlists':[likes,owned,published],'/api/player/me':{id:1,name:'Listener'},
            '/api/player/likes':{track_ids:songs.map(t=>t.id)},'/api/player/artists':{items:[],total:0,has_more:false},
        };
        return route.fulfill({json:replies[pathname]||[]});
    });
    const input=page.locator('#playlist-track-search'), clear=page.locator('.playlist-search-clear');
    const rows=page.locator('.playlist-track-row');
    const ids=async expected=>page.waitForFunction(expected=>{
        const current=[...document.querySelectorAll('.playlist-track-row')].map(el=>Number(el.dataset.playlistIndex));
        return JSON.stringify(current) === JSON.stringify(expected);
    },expected);
    const go = id => page.goto('http://filter.test/#playlist/'+id);
    try {
        await go(-1);
        await ids([0,1,2,3]);
        await input.fill('вЕсНа');
        await ids([0]);
        await input.fill('АРТИСТ СЕТИ');
        await ids([3]);
        await rows.getByText('Лунный свет',{exact:true}).waitFor();
        await input.fill('сНеГ');
        await ids([1]);
        assert.equal(await page.locator('#search-input').inputValue(),'');
        assert.equal(new URL(page.url()).hash,'#playlist/-1');
        await clear.click();
        await ids([0,1,2,3]);
        await input.fill('совсем ничего');
        await page.locator('.playlist-search-empty').waitFor();
        await ids([]);
        await go(7);
        await ids([0,1,2,3]);
        assert.equal(await input.inputValue(),'','opening a different playlist clears the filter');
        await page.evaluate(()=>{
            Alpine.store('queue').playRelease=(tracks,idx)=>{
                window.filteredSelection={idx, ids:tracks.map(t=>t.playlist_track_id)};
            };
        });
        await input.fill('СЕВЕР');
        await ids([1,3]);
        await rows.last().dblclick();
        assert.deepEqual(await page.evaluate(()=>window.filteredSelection),{idx:3,ids:[201,202,203,204]},
            'a later playlist entry with a repeated track ID plays at its original queue position');
        await clear.click();
        await ids([0,1,2,3]);
        const reduced=await input.evaluate(async el=>{
            el.value='снег'; el.dispatchEvent(new Event('input',{bubbles:true}));
            await Alpine.nextTick();
            const wrappers=[...document.querySelectorAll('.playlist-filter-row')];
            return [wrappers.length, wrappers.flatMap(row=>row.getAnimations()).length];
        });
        assert.deepEqual(reduced,[2,0],'reduced motion applies the filtered rows immediately');
        await clear.click();
        await ids([0,1,2,3]);
        await page.emulateMedia({reducedMotion:'no-preference'});
        for (const [query,sign,expected] of [['снег',-1,[1,3]],['',1,[0,1,2,3]]]) {
            const {measures,duration}=await sampleTransition(input,query,0);
            assert.ok(duration>=130 && duration<=160,'150 ms filter transition');
            for (const [column,metric] of [[0,'height'],[1,'opacity']]) {
                const [from,mid,to]=measures.map(item=>item[column]);
                assert.ok(sign*(mid-from)>0 && sign*(to-mid)>0,`${metric} changes smoothly during ${sign<0?'exit':'entry'}: ${measures}`);
            }
            await ids(expected);
        }
        await input.evaluate(async el=>{
            for (const q of ['снег','небо','снег','вокзал']) {
                el.value=q; el.dispatchEvent(new Event('input',{bubbles:true}));
                await new Promise(resolve=>requestAnimationFrame(resolve));
            }
        });
        await ids([2]);
        await page.waitForTimeout(200); // stale exit callbacks must not affect the last query
        assert.deepEqual(await rows.evaluateAll(list=>list.map(el=>el.dataset.playlistIndex)),['2']);
        await go(8);
        await ids([0,1,2]);
        await input.fill('СЕВЕР');
        await ids([1]);
        await page.goto('http://filter.test/share/playlist-filter-test');
        await ids([0,1,2]);
        assert.equal(await page.locator('.sidebar-left').count(),0,'shared link uses the guest shell');
        assert.equal(await input.inputValue(),'','a shared playlist starts with a fresh search');
        await input.fill('ГОСТЬ');
        await ids([0]);
        assert.deepEqual(searchRequests,[],'filtering uses the displayed playlist without querying the catalog or federation');
        assert.ok(await input.evaluate(el=>{
            const r=el.getBoundingClientRect();
            return r.left>=0 && r.right<=innerWidth+1 && document.documentElement.scrollWidth<=innerWidth+1;
        }),'the filter fits the viewport');
        assert.deepEqual(errors,[]);
    } finally {await page.close();}
}

(async()=>{
    const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
    try {
        for(const width of [1280,320]) await check(browser,width);
        console.log('Local playlist filters, animation, reduced motion, duplicates and playback identity passed on desktop/mobile.');
    } finally {await browser.close();}
})().catch(err=>{console.error(err);process.exitCode=1;});

const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const fs=require('node:fs'),assert=require('node:assert/strict');
const html=fs.readFileSync('target/podcast-player-fixture.html','utf8'),alpine=fs.readFileSync('target/podcast-alpine.min.js','utf8');
const local=(id,title='Still Loving You',artist='Scorpions')=>({id,title,artists:[{id:1,name:artist}],featured_artists:[],release_title:'Release '+id,release_id:id,duration_seconds:300+id,audio_format:'flac',lastfm_rating:87});
const remote=(id,title='Still Loving You')=>({key:{content_id:'b3:'+String(id).padStart(64,'0')},metadata:{title,artists:[{name:'Scorpions'}],featured_artists:[],release:{title:'Remote release '+id},duration_seconds:300+id},availability:{state:'federated',local:null,federation:[{owner:'peer',item_id:String(id)}]}});
const data={local:[local(1),local(2,'Another song'),local(3),local(4,'Still Loving You','Other artist'),local(5)],remote:[remote(11),remote(12),remote(13,'Other remote song')]};
(async()=>{
 const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
 try{
  for(const width of [1440,390,320]){
   const page=await browser.newPage({viewport:{width,height:1000}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
   await page.route('**/*',route=>{
    const u=new URL(route.request().url());
    if(u.hostname==='groups.test'&&u.pathname==='/')return route.fulfill({contentType:'text/html',body:html});
    if(u.hostname==='cdn.jsdelivr.net'&&u.pathname.includes('alpine'))return route.fulfill({contentType:'application/javascript',body:alpine});
    if(u.pathname==='/api/player/artists')return route.fulfill({json:{items:[],total:0,has_more:false}});
    return route.fulfill({json:[]});
   });
   await page.goto('http://groups.test/');await page.waitForFunction(()=>window.Alpine?.store('searchGroups')&&Alpine.store('library'));
   await page.evaluate(data=>{
    window.groupPlays=[];
    const library=Alpine.store('library');library.view='search';library.searchQuery='Still';library.searchLoading=false;library.similaritySearchLabel='';
    library.searchResults={artists:[],releases:[],tracks:data.local,podcasts:[]};library.federationSearch={loading:false,error:'',artists:[],releases:[],tracks:data.remote};
    Alpine.store('queue').playRelease=(tracks,index)=>window.groupPlays.push({source:'local',ids:tracks.map(t=>t.id),id:tracks[index].id});
    library.playFederatedTrack=track=>window.groupPlays.push({source:'federation',id:track.key.content_id});
   },data);
   const localRows=page.locator('[data-search-source="local"]'),remoteRows=page.locator('[data-search-source="federation"]');
   await page.waitForFunction(()=>document.querySelectorAll('[data-search-source="local"]').length===5);
   assert.equal(await localRows.count(),5);assert.equal(await remoteRows.count(),3);
   assert.equal(await localRows.locator('.search-version-toggle:visible').count(),1);
   assert.equal(await localRows.nth(1).getAttribute('inert')!==null,true);
   assert.ok((await localRows.nth(1).boundingBox()).height<1);assert.ok((await localRows.nth(2).boundingBox()).height<1);
   assert.equal(await localRows.first().locator('.search-version-toggle').getAttribute('aria-expanded'),'false');
   await localRows.first().locator('.search-track-name').click();
   assert.deepEqual(await page.evaluate(()=>window.groupPlays.at(-1)),{source:'local',ids:[1,2,4],id:1});
   await page.evaluate(()=>{window.groupPlays=[];window.groupNodes=[...document.querySelectorAll('[data-search-source="local"]')];});
   const animation=await page.evaluate(()=>new Promise(resolve=>{
    const head=document.querySelector('[data-search-source="local"]'),next=document.querySelectorAll('[data-search-source="local"]')[3],positions=[],start=performance.now();
    head.querySelector('.search-version-toggle').click();
    function sample(){positions.push(next.getBoundingClientRect().top);if(performance.now()-start<230)requestAnimationFrame(sample);else resolve(positions);}
    requestAnimationFrame(sample);
   }));
   assert.ok(new Set(animation.map(y=>Math.round(y))).size>2,'siblings move smoothly instead of jumping');
   assert.equal(await page.evaluate(()=>window.groupPlays.length),0,'branch toggle does not play');
   assert.equal(await localRows.first().locator('.search-version-toggle').getAttribute('aria-expanded'),'true');
   assert.equal(await localRows.nth(1).getAttribute('inert'),null);
   assert.equal(await page.evaluate(()=>window.groupNodes.every((node,index)=>node===document.querySelectorAll('[data-search-source="local"]')[index])),true,'stable DOM identities on toggle');
   for(let index=0;index<3;index++){
    const row=localRows.nth(index),box=await row.boundingBox(),actions=await row.locator('.track-actions').boundingBox();
    assert.ok(actions.x>=0&&actions.x+actions.width<=width,'actions fit viewport');assert.ok(actions.y>=box.y&&actions.y+actions.height<=box.y+box.height+1);
   }
   await localRows.nth(1).locator('.search-track-name').click();assert.equal(await page.evaluate(()=>window.groupPlays.at(-1).id),3,'expanded version plays its original content');
   await page.screenshot({path:`target/search-groups-expanded-${width}.png`});
   const closing=await page.evaluate(()=>new Promise(resolve=>{
    const rows=document.querySelectorAll('[data-search-source="local"]'),positions=[],start=performance.now();rows[0].querySelector('.search-version-toggle').click();
    function sample(){positions.push(rows[3].getBoundingClientRect().top);if(performance.now()-start<230)requestAnimationFrame(sample);else resolve(positions);}requestAnimationFrame(sample);
   }));
   assert.ok(new Set(closing.map(y=>Math.round(y))).size>2,'collapse also moves siblings smoothly');
   await page.waitForFunction(()=>document.querySelectorAll('[data-search-source="local"]')[1].getBoundingClientRect().height<1);
   assert.equal(await localRows.nth(1).getAttribute('aria-hidden'),'true');
   await localRows.first().locator('.search-version-toggle').focus();await page.keyboard.press('Enter');
   await page.waitForFunction(()=>document.querySelector('[data-search-source="local"] .search-version-toggle').getAttribute('aria-expanded')==='true');
   await page.evaluate(()=>new Promise(resolve=>{const button=document.querySelector('[data-search-source="local"] .search-version-toggle');[0,20,40,60,80].forEach(delay=>setTimeout(()=>button.click(),delay));setTimeout(resolve,280);}));
   assert.equal(await localRows.first().locator('.search-version-toggle').getAttribute('aria-expanded'),'false');
   assert.ok((await localRows.nth(1).boundingBox()).height<1,'rapid toggles settle in the final state');
   const remoteHead=remoteRows.first();await remoteHead.locator('.search-version-toggle').click();
   await page.evaluate(track=>Alpine.store('library').federationSearch.tracks.push(track),remote(14));
   await page.waitForFunction(()=>document.querySelectorAll('[data-search-source="federation"]').length===4);
   assert.equal(await remoteHead.locator('.search-version-toggle').getAttribute('aria-expanded'),'true');
   assert.equal(await remoteHead.locator('.search-version-toggle span').textContent(),'3');
   await remoteRows.nth(1).locator('.search-track-name').click();assert.equal(await page.evaluate(()=>window.groupPlays.at(-1).id),remote(12).key.content_id);
   assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
   await page.emulateMedia({reducedMotion:'reduce'});assert.equal(await localRows.first().evaluate(el=>getComputedStyle(el).transitionDuration),'0s');
   assert.deepEqual(errors,[]);await page.close();
  }
  console.log('Grouped local/federation search, animation, playback and streaming checks passed.');
 }finally{await browser.close();}
})().catch(error=>{console.error(error);process.exitCode=1;});

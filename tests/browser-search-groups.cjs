const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const fs=require('node:fs'),assert=require('node:assert/strict');
const http=require('node:http');
const html=fs.readFileSync('target/podcast-player-fixture.html','utf8'),alpine=fs.readFileSync('target/podcast-alpine.min.js','utf8');
const local=(id,title='Still Loving You',artist='Scorpions')=>({id,title,artists:[{id:1,name:artist}],featured_artists:[],release_title:'Release '+id,release_id:id,duration_seconds:300+id,audio_format:'flac',lastfm_rating:87});
const remote=(id,title='Still Loving You')=>({key:{content_id:'b3:'+String(id).padStart(64,'0')},metadata:{title,artists:[{name:'Scorpions'}],featured_artists:[],release:{title:'Remote release '+id},duration_seconds:300+id},availability:{state:'federated',local:null,federation:[{owner:'peer',item_id:String(id)}]}});
const data={local:[local(1),local(2,'Another song'),local(3),local(4,'Still Loving You','Other artist'),local(5)],remote:[remote(11),remote(12),remote(13,'Other remote song')]};
async function checkArtistPopularTracks(page) {
 await page.emulateMedia({reducedMotion:'no-preference'});
 const tracks = [
  local(11,'За закрытой дверью','Noize MC'), local(12,'За Закрытой Дверью','Noize MC & Guest'),
  ...Array.from({length:6}, (_, i) => local(13 + i, 'Song ' + i, 'Noize MC')),
 ];
 const artist = {id:397,name:'Noize MC',releases:[],top_tracks:tracks,featured_tracks:[],total_track_count:tracks.length};
 await page.route('**/api/player/artists/397', route => route.fulfill({json:artist}));
 await page.evaluate(() => { location.hash = 'artist/397'; });
 const section = page.locator('.artist-top-tracks');
 const rows = section.locator('.artist-appearance-row');
 const toggle = section.locator('.artist-top-tracks-toggle');
 await rows.first().waitFor();
 assert.equal(await rows.count(), 5, 'collapsed artist preview counts unique titles');
 assert.equal(await rows.first().locator('.search-version-toggle span').textContent(), '2');
 assert.equal(await toggle.getAttribute('aria-expanded'), 'false');
 const bounds = await toggle.boundingBox(), list = await section.locator('#artist-popular-tracks').boundingBox();
 assert.ok(bounds.y >= list.y + list.height - 1 && bounds.width >= list.width * .9 && bounds.height <= 32);
 await rows.first().press('Enter');
 assert.deepEqual(await page.evaluate(() => window.groupPlays.at(-1)), {source:'local',ids:[11,13,14,15,16,17,18],id:11});
 await rows.first().locator('.search-version-toggle').click();
 await rows.nth(1).press('Enter');
 assert.equal(await rows.count(), 6);
 assert.equal(await page.evaluate(() => window.groupPlays.at(-1).id), 12, 'variant plays its own recording');
 // Sample the running browser transition at its midpoint, independently of the CI frame rate.
 const samples = await section.evaluate(async el => {
  const button = el.querySelector('.artist-top-tracks-toggle'), tail = el.querySelector('.artist-popular-overflow');
  const measure = () => ({height:tail.getBoundingClientRect().height,
   button:button.getBoundingClientRect().top - el.getBoundingClientRect().top});
  async function sample() {
   const before = measure();
   button.click();
   await Alpine.nextTick();
   const slide = tail.getAnimations().find(a => a.transitionProperty === 'grid-template-rows');
   if (!slide) throw Error('Missing popular track list slide');
   const duration = slide.effect.getTiming().duration;
   slide.pause(); slide.currentTime = duration / 2;
   const middle = measure(), inert = tail.inert, mounted = tail.querySelectorAll('.track-row').length;
   slide.play(); await slide.finished;
   await Alpine.nextTick();
   return {before,middle,after:measure(),inert,mounted,duration};
  }
  return [await sample(),await sample()];
 });
 const [open,close] = samples;
 for (const [result,sign] of [[open,1],[close,-1]]) {
  assert.ok(result.duration >= 100 && result.duration <= 150);
  for (const key of ['height','button']) {
   assert.ok(sign * (result.middle[key] - result.before[key]) > 1, key+' changes during the slide');
   assert.ok(sign * (result.after[key] - result.middle[key]) > 1, key+' reaches its endpoint after the midpoint');
  }
 }
 assert.equal(close.mounted,2);
 assert.equal(close.inert,true);
 await rows.nth(5).waitFor({state:'detached'});
 assert.equal(await rows.count(),5);
 assert.equal(await rows.first().locator('.search-version-toggle').getAttribute('aria-expanded'), 'false');
 // Reverse an in-flight close and let its stale cleanup deadline pass.
 await toggle.click();
 await section.locator('.artist-popular-overflow').evaluate(el=>Promise.all(el.getAnimations().map(a=>a.finished)));
 await toggle.evaluate(async button => {
  button.click(); await Alpine.nextTick();
  const tail = button.parentNode.querySelector('.artist-popular-overflow');
  const slide = tail.getAnimations().find(a => a.transitionProperty === 'grid-template-rows');
  if (!slide) throw Error('Missing transition to reverse');
  slide.currentTime = slide.effect.getTiming().duration / 2;
  button.click();
 });
 await page.waitForTimeout(240);
 assert.equal(await rows.count(),7);
 assert.equal(await toggle.getAttribute('aria-expanded'),'true');
 await page.emulateMedia({reducedMotion:'reduce'});
 await toggle.press('Enter');
 await rows.nth(5).waitFor({state:'detached'});
 assert.equal(await section.locator('.artist-popular-overflow').evaluate(el=>el.getAnimations().length),0);
}

async function checkArtistScrollReturn(page) {
 const tracks = Array.from({length:35}, (_, i) => local(i+1, 'Track '+i));
 const releases = Array.from({length:48}, (_, i) => ({id:i+1,title:'Album '+i,release_type:'album',track_count:8}));
 await page.route('**/api/player/artists/398', route => route.fulfill({json:{
  id:398,name:'Scorpions',top_tracks:tracks,featured_tracks:[],releases,total_track_count:tracks.length,
 }}));
 await page.evaluate(() => { location.hash = 'artist/398'; });
 const section = page.locator('.artist-top-tracks');
 const toggle = section.getByRole('button', {name:'Expand all',exact:true});
 const collapse = section.getByRole('button', {name:'Collapse',exact:true});
 await page.getByText('Album 47',{exact:true}).waitFor({state:'attached'});
 const scroller = page.locator('#center-scroll');
 for (const reducedMotion of ['no-preference','reduce']) {
  await page.emulateMedia({reducedMotion});
  // Expand midway down the viewport, then scroll the distant Collapse button into a different position.
  const offset = await scroller.evaluate(el => {
   const button = el.querySelector('.artist-top-tracks-toggle');
   el.scrollTop += button.getBoundingClientRect().top - el.getBoundingClientRect().top - el.clientHeight * .5;
   return el.scrollTop;
  });
  const expandY = (await toggle.boundingBox()).y;
  await toggle.click();
  await section.locator('.artist-appearance-row').last().getByText('Track 34',{exact:true}).waitFor({state:'attached'});
  await section.locator('.artist-popular-overflow').evaluate(el => Promise.all(el.getAnimations().map(a => a.finished)));
  const expandedOffset = await scroller.evaluate(el => {
   const button = el.querySelector('.artist-top-tracks-toggle');
   el.scrollTop += button.getBoundingClientRect().top - el.getBoundingClientRect().top - el.clientHeight * .25;
   return el.scrollTop;
  });
  assert.ok(expandedOffset > offset + 1000, 'the listener has scrolled through the expanded tracks');
  await collapse.click();
  await section.locator('.artist-appearance-row').nth(5).waitFor({state:'detached'});
  await page.waitForFunction(y => Math.abs(document.querySelector('.artist-top-tracks-toggle').getBoundingClientRect().top - y) < 12,
   expandY, {timeout:2500});
 }
}
data.remote[0].metadata.artists[0].local_id=1;
data.remote[0].metadata.release.local_id=442;
data.remote[0].metadata.featured_artists=[{name:'Guest & Co'}];
data.remote[0].availability.local={track_id:111, stream_url:'/api/player/stream/111'};
(async()=>{
 const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
 const server=http.createServer((_req,res)=>{res.writeHead(200,{'Content-Type':'text/html; charset=utf-8'});res.end(html);});
 try{
  await new Promise((resolve,reject)=>server.once('error',reject).listen(0,'127.0.0.1',resolve));
  const origin='http://127.0.0.1:'+server.address().port;
  for(const width of [1440,390,320]){
   const page=await browser.newPage({viewport:{width,height:1000}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
   let finishSimilarity;
   const peersReady=new Promise(resolve=>{finishSimilarity=resolve;});
   await page.context().route('**/*',async route=>{
    const u=new URL(route.request().url());
    if(u.origin===origin&&u.pathname==='/')return route.continue();
    if(u.hostname==='cdn.jsdelivr.net'&&u.pathname.includes('alpine'))return route.fulfill({contentType:'application/javascript',body:alpine});
    if(u.pathname==='/api/player/artists')return route.fulfill({json:{items:[],total:0,has_more:false}});
    if(u.pathname==='/api/player/releases/442')return route.fulfill({json:{id:442,title:'Remote release 11',artists:[],tracks:[],uploaders:[]}});
    if(u.pathname==='/api/player/similarity/42'){
     if(u.searchParams.get('local_only'))return route.fulfill({json:{label:'Seed',tracks:[
      {...local(1),similarity_score:0.9},{...local(2,'Another song'),similarity_score:0.7},
     ]}});
     await peersReady;
     return route.fulfill({json:{queried_peers:1,federation_tracks:[{...remote(12),similarity_score:0.95}]}});
    }
    return route.fulfill({json:[]});
   });
   await page.goto(origin);await page.waitForFunction(()=>window.Alpine?.store('searchGroups')&&Alpine.store('library'));
   await page.evaluate(data=>{
    window.groupPlays=[];
    const library=Alpine.store('library');library.view='search';library.searchQuery='Still';library.searchLoading=false;library.similaritySearchLabel='';
    library.searchResults={artists:[],releases:[],tracks:data.local,podcasts:[]};library.federationSearch={loading:false,error:'',artists:[],releases:[],tracks:data.remote};
    library.updateUnifiedSearch('tracks');
    Alpine.store('queue').playRelease=(tracks,index)=>window.groupPlays.push({source:'local',ids:tracks.map(t=>t.id),id:tracks[index].id});
    library.toggleFederationLike=track=>window.likedContent=track.key.content_id;
    Alpine.store('playlists').showFederationPicker=track=>window.playlistContent=track.key.content_id;
   },data);
   const localRows=page.locator('[data-search-source="local"]'),remoteRows=page.locator('[data-search-source="federation"]');
   await page.waitForFunction(()=>document.querySelectorAll('[data-search-source="local"]').length===3);
   assert.equal(await localRows.count(),3);assert.equal(await remoteRows.count(),1);
   assert.equal(await localRows.first().locator('.search-federation-only').count(),0);
   assert.equal(await remoteRows.first().locator('.search-federation-only').count(),1);
   assert.equal(await page.locator('.federation-search-section:visible').count(),0);
   assert.equal(await localRows.locator('.search-version-toggle:visible').count(),1);
   assert.equal(await localRows.locator('.search-track-variant').count(),0,'collapsed versions are unmounted');
   assert.equal(await localRows.first().locator('.search-version-toggle').getAttribute('aria-expanded'),'false');
   await localRows.first().locator('.search-track-name').click();
   assert.deepEqual(await page.evaluate(()=>window.groupPlays.at(-1)),{source:'local',ids:[1,2,4,'fed:'+remote(13,'Other remote song').key.content_id],id:1});
   await page.evaluate(()=>{window.groupPlays=[];window.groupNodes=[...document.querySelectorAll('[data-search-source="local"]')];});
   const animation=await page.evaluate(()=>new Promise(resolve=>{
    const [head,next]=window.groupNodes,positions=[],start=performance.now();
    head.querySelector('.search-version-toggle').click();
    function sample(){positions.push(next.getBoundingClientRect().top);if(performance.now()-start<230)requestAnimationFrame(sample);else resolve(positions);}
    requestAnimationFrame(sample);
   }));
   assert.ok(new Set(animation.map(y=>Math.round(y))).size>2,'siblings move smoothly instead of jumping');
   assert.equal(await page.evaluate(()=>window.groupPlays.length),0,'branch toggle does not play');
   assert.equal(await localRows.first().locator('.search-version-toggle').getAttribute('aria-expanded'),'true');
   assert.equal(await localRows.nth(1).getAttribute('inert'),null);
   assert.equal(await localRows.count(),6);assert.equal(await remoteRows.count(),2);
   assert.equal(await localRows.nth(1).getByRole('link',{name:'Scorpions'}).getAttribute('href'),'#artist/1');
   assert.equal(await localRows.nth(1).getByRole('link',{name:'Release 3'}).getAttribute('href'),'#release/3');
   assert.equal(await page.evaluate(()=>window.groupNodes.every(node=>document.getElementById(node.id)===node)),true,'representatives keep their DOM identities');
   for(let index=0;index<3;index++){
    const row=localRows.nth(index),box=await row.boundingBox(),actions=await row.locator('.track-actions').boundingBox();
    assert.ok(actions.x>=0&&actions.x+actions.width<=width,'actions fit viewport');assert.ok(actions.y>=box.y&&actions.y+actions.height<=box.y+box.height+1);
   }
   await localRows.nth(1).locator('.search-track-name').click();assert.equal(await page.evaluate(()=>window.groupPlays.at(-1).id),3,'expanded version plays its original content');
   await page.screenshot({path:`target/search-groups-expanded-${width}.png`});
   const closing=await page.evaluate(()=>new Promise(resolve=>{
    const rows=document.querySelectorAll('[data-search-source="local"]'),positions=[],start=performance.now();rows[0].querySelector('.search-version-toggle').click();
    function sample(){positions.push(rows[4].getBoundingClientRect().top);if(performance.now()-start<230)requestAnimationFrame(sample);else resolve(positions);}requestAnimationFrame(sample);
   }));
   assert.ok(new Set(closing.map(y=>Math.round(y))).size>2,'collapse also moves siblings smoothly');
   await page.waitForFunction(()=>document.querySelectorAll('[data-search-source="local"]').length===3);
   assert.equal(await localRows.locator('.search-track-variant').count(),0);
   await localRows.first().locator('.search-version-toggle').focus();await page.keyboard.press('Enter');
   await page.waitForFunction(()=>document.querySelector('[data-search-source="local"] .search-version-toggle').getAttribute('aria-expanded')==='true');
   await page.evaluate(()=>new Promise(resolve=>{const button=document.querySelector('[data-search-source="local"] .search-version-toggle');[0,20,40,60,80].forEach(delay=>setTimeout(()=>button.click(),delay));setTimeout(resolve,280);}));
   assert.equal(await localRows.first().locator('.search-version-toggle').getAttribute('aria-expanded'),'false');
   assert.equal(await localRows.count(),3,'rapid toggles settle in the final state');
   await localRows.first().locator('.search-version-toggle').click();
   await page.evaluate(track=>{
    const lib=Alpine.store('library');lib.federationSearch.tracks.push(track);lib.updateUnifiedSearch('tracks');
   },remote(14));
   await page.waitForFunction(()=>document.querySelectorAll('[data-search-source="federation"]').length===3);
   assert.equal(await localRows.first().locator('.search-version-toggle').getAttribute('aria-expanded'),'true');
   assert.equal(await localRows.first().locator('.search-version-toggle span').textContent(),'6');
   const remoteVersion=remoteRows.filter({has:page.getByRole('link',{name:'Remote release 12',exact:true})});
   await remoteVersion.locator('.search-track-name').click();
   assert.equal(await page.evaluate(()=>window.groupPlays.at(-1).id),'fed:'+remote(12).key.content_id);
   const menu=page.locator('.track-action-menu:visible');
   await remoteVersion.locator('.like-btn').click();
   await remoteVersion.locator('.track-more-btn').click();
   await menu.waitFor({timeout:4000});
   assert.deepEqual((await menu.locator('[data-action]:visible').evaluateAll(items=>items.map(item=>item.dataset.action))).sort(),
    ['info','playlist','queue'],'remote-only audio exposes actions supported for that recording');
   await menu.locator('[data-action="playlist"]').click();
   assert.deepEqual(await page.evaluate(()=>[window.likedContent,window.playlistContent]),[remote(12).key.content_id,remote(12).key.content_id]);
   await remoteVersion.locator('.queue-next-btn').click();
   assert.equal(await page.evaluate(()=>Alpine.store('queue').tracks.some(track=>track.federation_pending && track.content_id===window.groupPlays.at(-1).id.slice(4))),true);
   await remoteVersion.locator('.track-more-btn').click();
   await menu.locator('[data-action="queue"]').click();
   assert.equal(await page.evaluate(()=>Alpine.store('queue').tracks.at(-1).content_id),remote(12).key.content_id);
   const remoteRelease=await remoteVersion.getByRole('link',{name:'Remote release 12'}).getAttribute('href');
   assert.ok(remoteRelease.startsWith('#releasefed?key='));
   assert.equal(JSON.parse(decodeURIComponent(remoteRelease.split('=')[1])).normalized_title,'remote release 12');
   assert.equal(await remoteVersion.getByRole('link',{name:'Scorpions'}).getAttribute('href'),'#artistfed?name=Scorpions');
   await remoteVersion.locator('.track-more-btn').click();
   await menu.locator('[data-action="info"]').click();
   assert.equal(await page.locator('.info-table').getByRole('link',{name:'Remote release 12'}).getAttribute('href'),remoteRelease);
   await page.locator('.info-modal-head button').click();
   const queued=await page.evaluate(()=>Alpine.store('library').federationQueueTrack(Alpine.store('library').federationSearch.tracks[0]));
   assert.equal(queued.id,111);
   assert.equal(queued.stream_url,'/api/player/stream/111');
   assert.equal(queued.release_id,442);
   assert.equal(queued.artists[0].id,1);
   assert.equal(await page.evaluate(()=>{
    const lib=Alpine.store('library');
    const track=lib.federationQueueTrack(lib.federationSearch.tracks[0]);
    return lib.releaseHref(lib.trackRelease({...track,release_id:1364,release_title:'Different local edition'}));
   }),'#release/1364','a local edition takes precedence over another release containing the same audio');
   assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
   await page.emulateMedia({reducedMotion:'reduce'});assert.equal(await localRows.first().evaluate(el=>getComputedStyle(el).transitionDuration),'0s');
   const plays=await page.evaluate(()=>window.groupPlays.length);
   const promoted=localRows.filter({has:page.getByRole('link',{name:'Remote release 11',exact:true})});
   assert.equal(await promoted.locator('.search-federation-only').count(),0);
   await promoted.locator('.track-more-btn').click();
   await menu.waitFor({timeout:4000});
   assert.equal(await menu.locator('[data-action]:visible:not(:disabled)').count(),6,'locally available peer audio supports all actions');
   await menu.locator('[data-action="info"]').click();
   const info=page.locator('.info-table');
   assert.equal(await info.getByRole('link',{name:'Scorpions'}).getAttribute('href'),'#artist/1');
   assert.equal(await info.getByRole('link',{name:'ft. Guest & Co'}).getAttribute('href'),'#artistfed?name=Guest%20%26%20Co');
   assert.equal(await info.getByText('1',{exact:true}).count(),1,'the local track info retains its federation peer count');
   if(width===1440){
    const [tab]=await Promise.all([page.context().waitForEvent('page'),info.getByRole('link',{name:'Remote release 11'}).click({modifiers:['Control']})]);
    await tab.waitForURL('**/#release/442');
    await tab.close();
    assert.equal(await info.count(),1,'modified click keeps the original result and modal open');
   }
   await info.getByRole('link',{name:'Remote release 11'}).click();
   await page.waitForURL('**/#release/442');
   assert.equal(await info.count(),0);
   assert.equal(await page.evaluate(()=>window.groupPlays.length),plays);
   await page.evaluate(()=>{
    Alpine.store('player').setVolume(0.37);
    Alpine.store('library').searchSimilar(42);
   });
   await page.waitForFunction(()=>Alpine.store('library').similaritySearchStats.loading && document.querySelectorAll('.search-standard-entry').length===2);
   assert.equal(await page.locator('.similarity-search-inline-progress').isVisible(),true);
   finishSimilarity();
   await page.waitForFunction(()=>!Alpine.store('library').similaritySearchStats.loading);
   const similar=page.locator('.search-standard-entry');
   assert.equal(await similar.count(),3,'similarity keeps versions ungrouped');
   assert.deepEqual(await similar.evaluateAll(rows=>rows.map(row=>row.dataset.searchSource)),['federation','local','local']);
   assert.equal(await similar.first().locator('.search-federation-only').count(),1);
   assert.equal(await similar.locator('.search-version-toggle:visible').count(),0);
   await similar.first().locator('.search-track-name').click();
   assert.deepEqual(await page.evaluate(()=>window.groupPlays.at(-1).ids),['fed:'+remote(12).key.content_id,1,2]);
   assert.equal(await page.evaluate(()=>window.groupPlays.at(-1).id),'fed:'+remote(12).key.content_id);
   assert.equal(await page.evaluate(()=>Alpine.store('player').volume),0.37);
   await checkArtistPopularTracks(page);
   await checkArtistScrollReturn(page);
   assert.deepEqual(errors,[]);await page.close();
  }
  console.log('Search and artist version groups, expansion, playback and streaming checks passed.');
 }finally{await browser.close();server.close();}
})().catch(error=>{console.error(error);process.exitCode=1;});

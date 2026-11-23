const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const fs=require('node:fs'),assert=require('node:assert/strict');
const html=fs.readFileSync('target/podcast-player-fixture.html','utf8'),alpine=fs.readFileSync('target/podcast-alpine.min.js','utf8');
const track=id=>({id,title:'Winter song',artists:[{id:1,name:'Artist'}],featured_artists:[],release_title:'Album',release_id:1,duration_seconds:240,lastfm_rating:87,has_lyrics:true});
(async()=>{
 const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
 try {
  for(const width of [320,390,1440]){
   const page=await browser.newPage({viewport:{width,height:900}}),errors=[],queries=[],federation=[];
   page.on('pageerror',e=>errors.push(e.message));
   await page.route('**/*',route=>{
    const u=new URL(route.request().url());
    if(u.hostname==='lyrics-search.test'&&u.pathname==='/')return route.fulfill({contentType:'text/html',body:html});
    if(u.hostname==='cdn.jsdelivr.net'&&u.pathname.includes('alpine'))return route.fulfill({contentType:'application/javascript',body:alpine});
    if(u.pathname==='/api/player/artists')return route.fulfill({json:{items:[],total:0,has_more:false}});
    if(u.pathname==='/api/player/search'){
     queries.push(u.searchParams);
     return route.fulfill({json:{artists:[{id:1,name:'Artist'}],releases:[{id:1,title:'Album',release_type:'Album'}],tracks:[track(1),track(2)],podcasts:[],lyrics_status:{ready_chunks:10}}});
    }
    if(u.pathname.includes('/federation/search'))federation.push(u.href);
    return route.fulfill({json:[]});
   });
   await page.goto('http://lyrics-search.test/');await page.waitForFunction(()=>window.Alpine?.store('library'));
   const input=page.locator('#search-input'),toggle=page.locator('.search-mode-toggle');
   await toggle.click();assert.equal(await toggle.getAttribute('aria-pressed'),'true');
   await page.waitForFunction(()=>getComputedStyle(document.querySelector('#search-input')).backgroundColor==='rgb(28, 41, 40)');
   assert.equal(await page.locator('.search-field').evaluate(el=>getComputedStyle(el,'::before').animationDuration),'12s');
   await input.fill('there will be no spring');await page.waitForTimeout(700);assert.equal(queries.length,0,'lyrics waits while composing');
   await input.press('Enter');await page.waitForFunction(()=>Alpine.store('library').searchResults?.tracks.length===2);
   assert.equal(queries.length,1);assert.equal(queries[0].get('mode'),'lyrics');assert.equal(federation.length,0);
   assert.equal(await page.locator('.search-artist-card:visible').count(),1);
   assert.equal(await page.locator('.search-release-card:visible').count(),1);
   assert.equal(await page.locator('.lyrics-search-snippet').count(),0,'ordinary results contain no lyrics fragments');
   await page.evaluate(()=>{Alpine.store('queue').playRelease=(tracks,index)=>{window.lastPlayed={ids:tracks.map(t=>t.id),index};};});
   await page.locator('[data-search-source="local"] .search-track-name').first().click();
   assert.deepEqual(await page.evaluate(()=>window.lastPlayed),{ids:[1],index:0});
   if(width<560){const box=await input.boundingBox(),panel=await page.locator('.content-topbar').boundingBox();assert.ok(Math.abs(box.width-panel.width)<1);}
   assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),'no horizontal overflow');
   await input.fill('new words');await page.waitForFunction(()=>Alpine.store('library').searchQuery==='new words');await page.waitForTimeout(2000);
   assert.equal(queries.length,2,'idle triggers one request');
   await page.emulateMedia({reducedMotion:'reduce'});assert.equal(await page.locator('.search-field').evaluate(el=>getComputedStyle(el,'::before').animationName),'none');
   await page.screenshot({path:`target/lyrics-search-${width}.png`});assert.deepEqual(errors,[]);await page.close();
  }
  console.log('Lyrics mode: debounce, Enter, local-only requests, standard grouped results, playback and responsive layout passed.');
 }finally{await browser.close();}
})().catch(error=>{console.error(error);process.exitCode=1;});

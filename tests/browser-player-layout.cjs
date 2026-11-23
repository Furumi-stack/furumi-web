// Render via FURU_PODCAST_RENDER_FIXTURE=1 cargo test renders_podcast_navigation_and_views.
const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const fs=require('node:fs'),assert=require('node:assert/strict');
const html=fs.readFileSync('target/podcast-player-fixture.html','utf8'),alpine=fs.readFileSync('target/podcast-alpine.min.js','utf8');
const tracks=[1,2,3].map(id=>({id,playlist_track_id:id,title:id===1?'Вся правда о Юрии Куклачёве — очень длинное название трека':'Time Only Knows',artists:[{id:1,name:'Длинное имя исполнителя'}],featured_artists:[],release_id:1,release_title:'Example album',duration_seconds:303,lastfm_rating:87,has_lyrics:id===1,stream_url:'/audio.mp3'}));
const near=(a,b,message)=>assert.ok(Math.abs(a-b)<1,`${message}: ${a} vs ${b}`);
async function checkRowMenu(page, row, track) {
 const more=row.locator('.track-more-btn'),menu=page.locator('.track-action-menu:visible');
 const activate=button=>page.viewportSize().width<=420 ? button.tap() : button.click();
 await more.focus();
 await page.keyboard.press('Enter');
 await menu.waitFor();
 const actions=menu.locator('[data-action]:visible');
 assert.deepEqual((await actions.evaluateAll(items=>items.map(item=>item.dataset.action))).sort(),
  ['download','info','playlist','queue','share','similar']);
 assert.equal(await actions.locator('svg').count(),6,'each action has an icon');
 const box=await menu.boundingBox(),viewport=page.viewportSize();
 assert.ok(box.x>=0 && box.y>=0 && box.x+box.width<=viewport.width+1 && box.y+box.height<=viewport.height+1,'the whole menu is reachable');
 await page.keyboard.press('Escape');
 await menu.waitFor({state:'hidden'});
 const choose=async action=>{
  await activate(more);
  await activate(menu.locator('[data-action="'+action+'"]'));
  await menu.waitFor({state:'hidden'});
 };
 await choose('info');
 const modal=page.locator('.info-modal');
 await modal.getByText(track.title,{exact:true}).waitFor();
 assert.equal(await modal.getByRole('button',{name:/download|find similar/i}).count(),0);
 await modal.locator('.info-modal-head button').click();
 await choose('playlist');
 assert.deepEqual(await page.evaluate(()=>Alpine.store('playlists').picker.trackIds),[track.id]);
 await page.locator('.modal-footer').getByRole('button',{name:'Cancel'}).click();
 await choose('queue');
 assert.equal(await page.evaluate(()=>Alpine.store('queue').tracks.at(-1)?.id),track.id);
 const shared=page.waitForRequest(req=>new URL(req.url()).pathname==='/api/player/shares'&&req.method()==='POST');
 await choose('share');
 assert.deepEqual((await shared).postDataJSON().track_ids,[track.id]);
 const downloaded=page.waitForEvent('download');
 await choose('download');
 assert.equal(new URL((await downloaded).url()).pathname,'/api/player/download/'+track.id);
}
(async()=>{
 const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
 try {
  for(const viewport of [{width:1440,height:1000},{width:900,height:1000},{width:820,height:1000},{width:575,height:900},{width:390,height:844},{width:320,height:740}]){
   const page=await browser.newPage({viewport,reducedMotion:'reduce',hasTouch:viewport.width<=420,isMobile:viewport.width<=420}),errors=[];page.on('pageerror',e=>errors.push(e.message));
   await page.route('**/*',route=>{
    const url=new URL(route.request().url());
    if(url.hostname==='layout.test'&&url.pathname==='/')return route.fulfill({contentType:'text/html',body:html});
    if(url.hostname==='cdn.jsdelivr.net'&&url.pathname.includes('alpine'))return route.fulfill({contentType:'application/javascript',body:alpine});
    if(url.pathname==='/api/player/artists')return route.fulfill({json:{items:[],total:0,has_more:false}});
    if(url.pathname==='/api/player/shares')return route.fulfill({json:{url:'/share/test'}});
    if(url.pathname.startsWith('/api/player/download/'))return route.fulfill({body:'test audio',contentType:'audio/mpeg',headers:{'Content-Disposition':'attachment; filename="test.mp3"'}});
    if(/^\/api\/player\/similarity\/[12]$/.test(url.pathname)){
     const seedId=Number(url.pathname.split('/').at(-1));
     return route.fulfill({json:url.searchParams.has('local_only')
      ? {label:tracks[seedId-1].title,tracks:[tracks[2]]}
      : {federation_tracks:[],queried_peers:0}});
    }
    return route.fulfill({json:[]});
   });
   await page.goto('http://layout.test/');await page.waitForFunction(()=>window.Alpine?.store('library')&&Alpine.store('player'));
   await page.evaluate(data=>{
    const library=Alpine.store('library');library.view='playlist_detail';library.currentPlaylist={id:-1,title:'Likes',kind:'likes',is_own:false,tracks:data};
    const player=Alpine.store('player');player.currentTrack=data[0];player.duration=303;player.currentTime=31;player.progress=10;
   },tracks);
   await page.waitForSelector('.playlist-track-row');
   const row=page.locator('.playlist-track-row').first(),pop=row.locator('.popularity-info-btn'),heart=row.locator('.like-btn');
   await page.evaluate(()=>{const rows=Alpine.store('library').currentPlaylist.tracks; rows[1].lastfm_rating=null; rows[2].lastfm_rating=103;});
   await page.locator('.playlist-track-row').nth(1).locator('.track-popularity-value').waitFor({state:'hidden',timeout:3000});
   const scoreCenters=await page.locator('.playlist-track-row .track-popularity-value:visible').evaluateAll(els=>els.map(el=>{
    const r=el.getBoundingClientRect();return r.x+r.width/2;
   }));
   assert.equal(scoreCenters.length,2);
   near(...scoreCenters,'two- and three-digit scores align');
   for(const selector of ['.like-btn','.queue-next-btn','.track-more-btn']){
    const positions=await page.locator('.playlist-track-row '+selector).evaluateAll(els=>els.map(el=>el.getBoundingClientRect().x));
    assert.equal(positions.length,3);
    for(const x of positions.slice(1))near(x,positions[0],'aligned '+selector+' across empty and populated scores');
   }
   if(viewport.width===1440||viewport.width===320){
    await checkRowMenu(page,row,tracks[0]);
    const queueButton=page.getByRole('button',{name:'Queue',exact:true});
    await queueButton.click();
    const queued=page.locator('.queue-panel .queue-track[data-queue-index="0"]');
    await queued.locator('.track-more-btn').click();
    const queueMenu=page.locator('.track-action-menu:visible');
    await queueMenu.locator('[data-action="info"]').click();
    await page.locator('.info-modal').getByText(tracks[0].title,{exact:true}).waitFor();
    await page.locator('.info-modal-head button').click();
    await queued.locator('.queue-track-remove').click();
    await page.locator('.queue-panel .queue-track[data-queue-index="0"]').waitFor({state:'detached'});
    if(viewport.width===320)await page.locator('.queue-backdrop').click({position:{x:4,y:4}});
    else await queueButton.click();
   }
   if(viewport.width===1440){
    const title=row.locator('.track-title');
    await page.evaluate(()=>{
     Alpine.store('queue').playRelease=(tracks,index)=>{window.playedFromRow=tracks[index].id;};
    });
    await title.dblclick({position:{x:12,y:8}});
    assert.equal(await page.evaluate(()=>window.playedFromRow),tracks[0].id,'double click plays the row');
    assert.equal(await page.evaluate(()=>getSelection().toString()),'','playing does not select the title');
    const box=await title.boundingBox();
    await page.mouse.move(box.x+2,box.y+8);
    await page.mouse.down();await page.mouse.move(box.x+56,box.y+8,{steps:8});await page.mouse.up();
    assert.ok((await page.evaluate(()=>getSelection().toString())).trim(),'drag still selects title text');
    await page.evaluate(()=>getSelection().removeAllRanges());
   }
   const initial=await pop.boundingBox(),heartBefore=await heart.boundingBox();
   const icon=await row.locator('.lyrics-presence').boundingBox();assert.ok(icon.x+icon.width<=initial.x,'lyrics appear to the left of popularity');
   await page.evaluate(()=>Alpine.store('library').currentPlaylist.tracks[0].has_lyrics=false);
   await row.locator('.lyrics-presence').waitFor({state:'hidden'});
   near((await pop.boundingBox()).x,initial.x,'popularity must not move');near((await heart.boundingBox()).x,heartBefore.x,'other actions must not move');
   await page.evaluate(()=>Alpine.store('library').currentPlaylist.tracks[0].has_lyrics=true);await row.locator('.lyrics-presence').waitFor({state:'visible'});
   assert.equal(await page.locator('.lyrics-toggle:visible').count(),1,'only one player lyrics button');
   const current=page.locator('.player-current-track');
   const similar=current.getByRole('button',{name:'Find similar tracks',exact:true});
   const bounds=await current.boundingBox(),button=await similar.boundingBox();
   assert.ok(button&&button.x>=bounds.x && button.x+button.width<=bounds.x+bounds.width+1,'similarity button fits the now-playing area');
   for(const selector of ['.player-track-title','.player-current-like','.player-current-share']){
    const neighbor=await current.locator(selector).boundingBox();
    assert.ok(neighbor.x+neighbor.width<=button.x+1 || button.x+button.width<=neighbor.x+1,'similarity button does not overlap '+selector);
   }
   const actions=await row.locator('.track-actions').boundingBox(),info=await row.locator('.playlist-track-info').boundingBox(),rowBox=await row.boundingBox();
   assert.ok(actions.y>=rowBox.y&&actions.y+actions.height<=rowBox.y+rowBox.height+1,'actions stay inside row');
   near(actions.y+actions.height/2,info.y+info.height/2,'playlist actions share title row');
   assert.ok(actions.x>=0&&actions.x+actions.width<=viewport.width,'actions fit viewport');
   assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false,'no horizontal page overflow');
   if(viewport.width<=900){
    assert.equal(await page.locator('.player-right > .lyrics-toggle:visible').count(),0);
    const volume=await page.locator('.volume-control').boundingBox(),right=await page.locator('.player-right').boundingBox();
    assert.ok(volume.x+volume.width<=right.x+right.width,'volume stays inside controls');
    const device=await page.locator('.player-right > .device-picker').boundingBox();
    near(device.y+device.height/2,volume.y+volume.height/2,'device control stays on volume row');
    assert.ok(device.x+device.width<=viewport.width,'device picker stays inside viewport');
   }else{
    const duration=await row.locator('.track-duration').boundingBox();assert.ok(actions.x+actions.width<=duration.x+1,'actions do not overlap duration');
   }
   await page.screenshot({path:`target/player-layout-likes-${viewport.width}.png`});
   await page.evaluate(()=>{const playlist=Alpine.store('library').currentPlaylist;playlist.id=12;playlist.kind='user';playlist.is_own=true;playlist.title='Playlist';Alpine.store('playlists').editingPlaylistId=12;});
   await page.waitForSelector('.playlist-track-row.editing');
   if(viewport.width===1440||viewport.width===320)await checkRowMenu(page,page.locator('.playlist-track-row').nth(1),tracks[1]);
   const editingActions=await row.locator('.track-actions').boundingBox(),editingInfo=await row.locator('.playlist-track-info').boundingBox();
   near(editingActions.y+editingActions.height/2,editingInfo.y+editingInfo.height/2,'editing actions stay in title row');
   if(viewport.width<=900){
    await page.evaluate(()=>Alpine.store('mobile').playerExpanded=true);await page.waitForSelector('.player-bar.mobile-expanded');
    assert.equal(await page.locator('.lyrics-toggle:visible').count(),1,'expanded player has one lyrics button');
    const expandedButtons=await page.locator('.player-buttons').boundingBox();assert.ok(expandedButtons.x>=-1&&expandedButtons.x+expandedButtons.width<=viewport.width+1,`expanded transport buttons fit viewport: ${JSON.stringify({viewport,expandedButtons})}`);
    await page.screenshot({path:`target/player-layout-expanded-${viewport.width}.png`});
   }
   for(const seed of tracks.slice(0,2)){
    await page.evaluate(track=>{Alpine.store('player').currentTrack=track;},seed);
    if(seed.id===1){
     if(viewport.width<=900)await page.evaluate(()=>{Alpine.store('mobile').playerExpanded=false;});
     await row.locator('.track-more-btn').click();
     await page.locator('.track-action-menu:visible [data-action="similar"]').click();
    }else{
     if(viewport.width<=900)await page.evaluate(()=>{Alpine.store('mobile').playerExpanded=true;});
     await similar.click();
    }
    await page.waitForURL('**/#similar/'+seed.id);
    await page.locator('.search-standard-entry').getByText(tracks[2].title,{exact:true}).first().waitFor();
    assert.equal(await page.evaluate(()=>Alpine.store('mobile').playerExpanded),false);
    assert.equal(await page.evaluate(()=>Alpine.store('player').currentTrack.id),seed.id);
   }
   await page.evaluate(()=>{Alpine.store('player').currentTrack={...Alpine.store('player').currentTrack,podcast:true};});
   await similar.waitFor({state:'hidden'});
   assert.deepEqual(errors,[]);await page.close();
  }
  console.log('Player layout checks passed at 1440, 900, 820, 575, 390 and 320 px.');
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});

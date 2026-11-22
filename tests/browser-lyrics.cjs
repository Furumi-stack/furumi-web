// Render with FURU_PODCAST_RENDER_FIXTURE=1 cargo test renders_podcast_navigation_and_views.
const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const fs=require('node:fs'),assert=require('node:assert/strict');
const html=fs.readFileSync('target/podcast-player-fixture.html','utf8');
const alpine=fs.readFileSync('target/podcast-alpine.min.js','utf8');
const track={id:1,title:'A song with synchronized lyrics',artists:[{id:1,name:'Example artist'}],duration_seconds:120,has_lyrics:true,release_title:'Album'};
const text=Array.from({length:24},(_,i)=>`[00:${String(i*2).padStart(2,'0')}.00] ${i===3?'<img src=x onerror=alert(1)>':`Line ${i+1} of the song`}`).join('\n');
(async()=>{
 const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
 try {
  for(const viewport of [{width:1440,height:1000},{width:390,height:844}]){
   const page=await browser.newPage({viewport,reducedMotion:'reduce'}),errors=[];
   page.on('pageerror',e=>errors.push(e.message));
   await page.route('**/*',async route=>{
    const u=new URL(route.request().url());
    if(u.hostname==='lyrics-ui.test'&&u.pathname==='/') return route.fulfill({contentType:'text/html',body:html});
    if(u.hostname==='cdn.jsdelivr.net'&&u.pathname.includes('alpine'))return route.fulfill({contentType:'application/javascript',body:alpine});
    if(u.pathname==='/api/player/lyrics/1')return route.fulfill({json:{versions:[{id:'plain',label:'Plain translation',plain_text:'A plain text translation',source:'manual'},{id:'timed',label:'Original',synced_lrc:text,source:'lrclib'}]}});
    if(u.pathname==='/api/player/lyrics/2')return route.fulfill({json:{versions:[]}});
    if(u.pathname==='/api/player/lyrics/3')return route.fulfill({json:{versions:[{id:'plain',label:'Plain',plain_text:'Third track lyrics\nAnother line',source:'manual'}]}});
    if(u.pathname==='/api/player/artists')return route.fulfill({json:{items:[],total:0,has_more:false}});
    if(u.pathname.startsWith('/api/'))return route.fulfill({json:[]});
    return route.fulfill({status:404,body:''});
   });
   await page.goto('http://lyrics-ui.test/');
   await page.waitForFunction(()=>window.Alpine?.store('lyrics')&&Alpine.store('player'));
   await page.evaluate(t=>{Alpine.store('player').currentTrack=t;Alpine.store('player').currentTime=0;},track);
   if(viewport.width<700) await page.evaluate(()=>Alpine.store('mobile').playerExpanded=true);
   await page.locator('.lyrics-toggle:visible').first().click();
   await page.waitForSelector('.lyrics-line').catch(async error=>{console.error({viewport,errors,state:await page.evaluate(()=>({visible:Alpine.store('lyrics').visible,loading:Alpine.store('lyrics').loading,error:Alpine.store('lyrics').error,track:Alpine.store('lyrics').track,versions:Alpine.store('lyrics').versions,lines:Alpine.store('lyrics').lines}))});await page.screenshot({path:'target/lyrics-error.png'});throw error;});
   assert.equal(await page.locator('.lyrics-fullscreen').count(),1);
   assert.equal(await page.locator('.lyrics-line').count(),24);
   assert.equal(await page.locator('.lyrics-line img').count(),0,'lyrics are text, never HTML');
   await page.evaluate(()=>Alpine.store('player').currentTime=30);
   await page.waitForFunction(()=>document.querySelector('.lyrics-line.active')?.textContent==='Line 16 of the song');
   await page.waitForFunction(()=>document.querySelector('.lyrics-scroll').scrollTop>100);
   await page.screenshot({path:`target/lyrics-${viewport.width}.png`,fullPage:true});
   const bounds=await page.locator('.lyrics-dialog').boundingBox();
   assert.equal(Math.round(bounds.width),viewport.width);assert.equal(Math.round(bounds.height),viewport.height);
   assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
   await page.selectOption('#lyrics-version','1');
   await page.waitForFunction(()=>document.querySelector('.lyrics-plain')?.textContent==='A plain text translation');
   await page.evaluate(()=>{Alpine.store('player').currentTrack={id:2,title:'No lyrics',artists:[]};Alpine.store('player').currentTime=0;});
   await page.waitForFunction(()=>Alpine.store('lyrics').loadedKey==='2'&&!Alpine.store('lyrics').loading);
   assert.equal(await page.locator('.lyrics-fullscreen').count(),1);assert.equal(await page.locator('.lyrics-line').count(),0);
   await page.evaluate(()=>Alpine.store('player').currentTrack={id:3,title:'Plain lyrics track',artists:[]});
   await page.waitForFunction(()=>document.querySelector('.lyrics-plain')?.textContent.includes('Third track'));
   await page.keyboard.press('Escape');assert.equal(await page.locator('.lyrics-overlay').count(),0);
   await page.evaluate(t=>Alpine.store('library').openTrackInfo(t),track);
   await page.getByRole('button',{name:'Available · view',exact:true}).click();
   await page.waitForSelector('.lyrics-line');
   assert.equal(await page.locator('.lyrics-fullscreen').count(),0);
   assert.ok((await page.locator('.lyrics-dialog').boundingBox()).height<viewport.height);
   assert.deepEqual(errors,[]);
   await page.close();
  }
  console.log('Lyrics desktop/mobile browser checks passed.');
 } finally {await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});

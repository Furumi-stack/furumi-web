const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const fs=require('node:fs'),assert=require('node:assert/strict');
const html=fs.readFileSync('target/lyrics-admin-fixture.html','utf8'),alpine=fs.readFileSync('target/podcast-alpine.min.js','utf8');
(async()=>{
 const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
 try {
  for(const viewport of [{width:1920,height:1080},{width:1440,height:1000},{width:390,height:844}]){
   const page=await browser.newPage({viewport}),errors=[];page.on('pageerror',e=>errors.push(e.message));
   await page.route('**/*',route=>{
    const url=new URL(route.request().url());
    if(url.hostname==='lyrics-admin.test'&&url.pathname==='/')return route.fulfill({contentType:'text/html',body:html});
    if(url.hostname==='cdn.jsdelivr.net'&&url.pathname.includes('alpine'))return route.fulfill({contentType:'application/javascript',body:alpine});
    if(url.hostname==='cdn.jsdelivr.net')return route.fulfill({contentType:'application/javascript',body:'window.lucide={createIcons(){}};'});
    return route.fulfill({json:{items:[],total:0}});
   });
   await page.goto('http://lyrics-admin.test/');await page.waitForFunction(()=>window.Alpine?.$data(document.querySelector('.admin-shell'))?.routeReady);
   await page.evaluate(()=>{
    const app=Alpine.$data(document.querySelector('.admin-shell'));
    app.activeLibraryItem={kind:'tracks',id:1,title:'Example song'};
    app.editorDetail={item:app.activeLibraryItem,metadata_tags:[],artists:[],releases:[]};
    app.editorDraft={title:'Example song',hidden:'false',artist_ids:[],release_id:1,release_tracks:[]};
    app.editorLyrics=[{id:'original',label:'Original',source:'lrclib',source_id:'123',language:'en',plain_text:'Words of a song',synced_lrc:'[00:12.50]Words of a song',instrumental:false}];
    app.editorLoading=false;app.editorOpen=true;
   });
   await page.waitForSelector('.lyrics-editor textarea');
   await page.locator('.lyrics-editor').getByRole('button',{name:'Add version',exact:true}).click();
   await page.waitForFunction(()=>document.querySelectorAll('.lyrics-editor-version').length===2);
   assert.equal(await page.locator('.lyrics-editor-version').count(),2);
   await page.locator('.lyrics-editor-version').nth(1).getByLabel('Plain text',{exact:true}).fill('A second version');
   await page.locator('.lyrics-editor').getByText('Retry LRCLIB lookup on the next run',{exact:false}).click();
   assert.equal(await page.evaluate(()=>Alpine.$data(document.querySelector('.admin-shell')).editorLyricsRetry),true);
   const checkbox=await page.locator('.lyrics-editor input[type=checkbox]').first().boundingBox();assert.ok(checkbox.width<=20&&checkbox.height<=20);
   await page.locator('.lyrics-editor').scrollIntoViewIfNeeded();
   await page.screenshot({path:`target/lyrics-admin-${viewport.width}.png`,fullPage:true});
   const outside=await page.locator('.lyrics-editor-version').evaluateAll(elements=>elements.some(el=>el.getBoundingClientRect().right>innerWidth));assert.equal(outside,false);
   await page.evaluate(()=>{const app=Alpine.$data(document.querySelector('.admin-shell'));app.editorOpen=false;app.activeView='settings';});
   const toggle=page.locator('#settings-lyrics input[type=checkbox]');await toggle.waitFor({state:'visible'});
   const toggleBox=await toggle.boundingBox();assert.ok(toggleBox.width<=20&&toggleBox.height<=20);
   assert.equal(await toggle.isChecked(),true);
   const layout=await page.locator('.settings-layout').boundingBox(),lyrics=await page.locator('#settings-lyrics').boundingBox();
   assert.ok(lyrics.width>=layout.width*0.9,'lyrics must occupy the full settings row');
   assert.equal(await page.locator('#settings-lyrics').evaluate(el=>getComputedStyle(el).gridArea),'lyrics');
   await page.locator('#settings-lyrics').scrollIntoViewIfNeeded();
   await page.screenshot({path:`target/lyrics-settings-${viewport.width}.png`,fullPage:true});
   assert.deepEqual(errors,[]);await page.close();
  }
  console.log('Lyrics admin desktop/mobile checks passed.');
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});

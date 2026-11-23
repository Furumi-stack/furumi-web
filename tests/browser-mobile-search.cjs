const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const fs=require('node:fs'),assert=require('node:assert/strict');
const html=fs.readFileSync('target/podcast-player-fixture.html','utf8'),alpine=fs.readFileSync('target/podcast-alpine.min.js','utf8');
const near=(a,b,message)=>assert.ok(Math.abs(a-b)<1,`${message}: ${a} vs ${b}`);
(async()=>{
 const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
 try{
  for(const width of [320,390,560,820,1440]){
   const page=await browser.newPage({viewport:{width,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
   await page.route('**/*',route=>{
    const u=new URL(route.request().url());
    if(u.hostname==='search.test'&&u.pathname==='/')return route.fulfill({contentType:'text/html',body:html});
    if(u.hostname==='cdn.jsdelivr.net'&&u.pathname.includes('alpine'))return route.fulfill({contentType:'application/javascript',body:alpine});
    if(u.pathname==='/api/player/artists')return route.fulfill({json:{items:[],total:0,has_more:false}});
    if(u.pathname==='/api/player/search')return route.fulfill({json:{artists:[],releases:[],tracks:[],podcasts:[]}});
    return route.fulfill({json:[]});
   });
   await page.goto('http://search.test/');await page.waitForFunction(()=>window.Alpine?.store('library'));
   const input=page.locator('#search-input'),panel=page.locator('.content-topbar');
   const original=await input.boundingBox(),panelBefore=await panel.boundingBox(),buttonBefore=await page.locator('.torrent-import-btn').boundingBox();
   await input.focus();
   if(width<=560){
    await page.waitForFunction(()=>{const input=document.querySelector('#search-input').getBoundingClientRect(),panel=document.querySelector('.content-topbar').getBoundingClientRect();return Math.abs(input.width-panel.width)<1;});
    const expanded=await input.boundingBox();near(expanded.x,panelBefore.x,'expanded left edge');near(expanded.width,panelBefore.width,'expanded width');
    assert.ok(expanded.width>original.width+80,'search gains the space of neighboring controls');
    const topmost=await page.evaluate(()=>{const panel=document.querySelector('.content-topbar').getBoundingClientRect();return document.elementFromPoint(panel.right-10,panel.top+panel.height/2)?.id;});
    assert.equal(topmost,'search-input','search overlays account and download controls');
    const duration=await page.locator('.search-field').evaluate(el=>parseFloat(getComputedStyle(el).transitionDuration));assert.ok(duration>0&&duration<=0.2);
    if(buttonBefore){const after=await page.locator('.torrent-import-btn').boundingBox();near(after.x,buttonBefore.x,'neighboring controls remain in place');}
    await input.fill('A long search query');await page.locator('.search-clear').click();
    assert.equal(await input.inputValue(),'');assert.equal(await input.evaluate(el=>document.activeElement===el),true,'clear retains input focus');
    await page.screenshot({path:`target/mobile-search-focused-${width}.png`});
    await input.evaluate(el=>el.blur());
    await page.waitForFunction(expected=>Math.abs(document.querySelector('#search-input').getBoundingClientRect().width-expected)<1,original.width);
    near((await input.boundingBox()).x,original.x,'blur restores position');near((await panel.boundingBox()).height,panelBefore.height,'header height remains stable');
    await page.emulateMedia({reducedMotion:'reduce'});assert.equal(await page.locator('.search-field').evaluate(el=>getComputedStyle(el).transitionDuration),'0s');
   }else{near((await input.boundingBox()).width,original.width,'larger screens retain normal search width');}
   assert.deepEqual(errors,[]);await page.close();
  }
  console.log('Mobile search expansion, overlay, clear, blur and reduced-motion checks passed.');
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});

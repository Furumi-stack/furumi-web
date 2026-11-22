// Render first: FURU_PODCAST_RENDER_FIXTURE=1 cargo test renders_podcast_navigation_and_views
// Uses the real rendered application with isolated API/feed fixtures; no user data.
const {chromium}=require(process.env.PLAYWRIGHT_MODULE||'playwright');
const fs=require('node:fs'),path=require('node:path'),assert=require('node:assert/strict');
const root=path.resolve(__dirname,'..');
const html=fs.readFileSync(path.join(root,'target/podcast-player-fixture.html'),'utf8');
const alpinePath=process.env.ALPINE_FIXTURE||path.join(root,'target/podcast-alpine.min.js');
const cover='https://feeds.example.test/cover.svg';
const description='A long RSS description, shown only on the detail page. '.repeat(150);
const mine=[
    {id:'pod1:own',metadata:{title:'A podcast about sound',author:'Sound studio',description,image_url:cover},episodes:2,new_episodes:1,shared:false},
    {id:'pod1:broken',metadata:{title:'A missing cover',author:'Test publisher',image_url:'https://feeds.example.test/missing.jpg'},episodes:0,new_episodes:0,shared:false},
    {id:'pod1:none',metadata:{title:'No RSS artwork',author:null},episodes:0,new_episodes:0,shared:false},
];
const publicPod={id:'pod1:public',url:'https://feeds.example.test/public.xml',metadata:{title:'Public podcast',author:'Another publisher',description,image_url:cover},subscribed:false};
const episode={id:'ep1:one',metadata:{title:'First episode',podcast_id:'pod1:own',published_at_ms:1700000000000,duration_ms:60000},cached:false,downloading:false,position_ms:0,completed:false,error:null};

(async()=>{
    const browser=await chromium.launch({headless:true,...(process.env.CHROME_PATH?{executablePath:process.env.CHROME_PATH}:{})});
    try{
        const page=await browser.newPage({viewport:{width:1440,height:1000}}),errors=[],actions=[];
        page.on('pageerror',error=>errors.push(error.message));
        await page.route('**/*',async route=>{
            const url=new URL(route.request().url());
            if(url.hostname==='cdn.jsdelivr.net'&&url.pathname.includes('alpine')&&fs.existsSync(alpinePath))return route.fulfill({contentType:'application/javascript',body:fs.readFileSync(alpinePath,'utf8')});
            if(url.hostname==='feeds.example.test')return url.pathname==='/cover.svg'?route.fulfill({contentType:'image/svg+xml',body:'<svg xmlns="http://www.w3.org/2000/svg" width="400" height="400"><rect width="400" height="400" fill="#244d43"/><circle cx="200" cy="175" r="85" fill="#d8ba78"/><text x="200" y="315" text-anchor="middle" fill="white" font-family="sans-serif" font-size="32">SOUND</text></svg>'}):route.fulfill({status:404,body:''});
            if(url.hostname==='podcast-ui.test'&&url.pathname==='/')return route.fulfill({contentType:'text/html',body:html});
            if(url.pathname==='/api/podcasts/action'){
                const body=route.request().postDataJSON();actions.push(body);
                if(body.action==='list')return route.fulfill({json:{mine,public:[publicPod]}});
                if(body.action==='episodes')return route.fulfill({json:{episodes:[{...episode,metadata:{...episode.metadata,podcast_id:body.id}}],has_more:false}});
                if(body.action==='add'){mine.push({...publicPod,episodes:1,new_episodes:1,shared:false});publicPod.subscribed=true;return route.fulfill({json:{ok:true,id:publicPod.id}});}
                return route.fulfill({json:{ok:true}});
            }
            if(url.pathname==='/api/player/search')return route.fulfill({json:{artists:[{id:1,name:'A musician',image_url:cover,release_count:1,track_count:1}],releases:[],tracks:[],podcasts:[publicPod,mine[0]]}});
            if(url.pathname.startsWith('/api/'))return route.fulfill({json:[]});
            if(url.hostname==='cdn.jsdelivr.net')return route.continue();
            return route.fulfill({status:404,body:''});
        });
        const settled=async view=>{await page.waitForFunction(v=>window.Alpine?.store('library')?.view===v&&!Alpine.store('podcasts').loading,view);};
        await page.goto('http://podcast-ui.test/#podcasts');await settled('podcasts');
        assert.equal(await page.locator('.card-grid .podcast-tile').count(),4);
        assert.equal(await page.locator('.podcast-description').count(),0);
        await page.waitForFunction(()=>document.querySelector('.podcast-tile img')?.naturalWidth>0);
        await page.waitForFunction(()=>Alpine.store('podcasts').failedArtwork['https://feeds.example.test/missing.jpg']);
        assert.equal(await page.locator('.podcast-tile[title="A missing cover"] svg').count(),1);
        await page.screenshot({path:path.join(root,'target/podcast-ui-grid.png')});

        await page.locator('.podcast-tile[title="A podcast about sound"]').click();await settled('podcast_detail');
        assert.ok(page.url().endsWith('#podcast?id=pod1%3Aown'));
        await page.getByRole('heading',{name:'First episode',exact:true}).waitFor();
        assert.equal(await page.locator('.podcast-detail > template').count(),1);
        assert.ok((await page.locator('.podcast-detail .podcast-description').first().innerText()).length<400);
        await page.screenshot({path:path.join(root,'target/podcast-ui-detail.png')});
        await page.getByRole('button',{name:'Show more',exact:true}).click();
        assert.equal(await page.locator('.podcast-detail .podcast-description').first().innerText(),description.trim());
        await page.getByRole('button',{name:'Show less',exact:true}).click();
        await page.reload();await settled('podcast_detail');
        await page.getByRole('heading',{name:'First episode',exact:true}).waitFor();
        await page.goBack();await settled('podcasts');

        await page.locator('.podcast-tile[title="Public podcast"]').click();await settled('podcast_detail');
        assert.equal(actions.filter(a=>a.action==='add').length,0);
        await page.getByRole('button',{name:'Subscribe',exact:true}).click();
        await page.getByRole('button',{name:'Publish',exact:true}).waitFor();
        assert.equal(actions.filter(a=>a.action==='add').length,1);

        await page.evaluate(()=>Alpine.store('library').search('sound'));
        await page.locator('.search-release-card.podcast-tile').first().waitFor();
        const headings=await page.locator('.search-section-title').allTextContents();
        assert.ok(headings.indexOf('Artists')<headings.indexOf('Podcasts'));
        assert.equal(await page.locator('.search-section .podcast-description').count(),0);
        const size=await page.locator('.search-release-card.podcast-tile').first().boundingBox();assert.ok(size.height<240&&size.width<=150);
        await page.screenshot({path:path.join(root,'target/podcast-ui-search.png')});

        await page.setViewportSize({width:390,height:844});
        await page.goto('http://podcast-ui.test/#podcasts');await settled('podcasts');
        await page.screenshot({path:path.join(root,'target/podcast-ui-mobile.png')});
        await page.locator('.podcast-tile[title="A podcast about sound"]').click();await settled('podcast_detail');
        assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
        await page.screenshot({path:path.join(root,'target/podcast-ui-mobile-detail.png')});
        assert.deepEqual(errors,[]);
        console.log('PASS: cold list/detail hashes, back navigation, RSS artwork/fallbacks, explicit subscribe, compact ordered search, desktop/mobile');
    } finally {await browser.close();}
})().catch(error=>{console.error(error);process.exitCode=1;});

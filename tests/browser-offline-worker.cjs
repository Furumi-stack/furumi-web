// Real service worker and Cache Storage. The HTTP fixture records every origin request.
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const player = path.join(__dirname, '..', 'src', 'player');
const catalog = fs.readFileSync(path.join(player, 'offline-catalog.js'), 'utf8');

(async () => {
    let account = '17', unavailable = false, authOutage = false, hangingRequest;
    const log = [];
    const server = http.createServer((req,res) => {
        const url = new URL(req.url, 'http://localhost');
        log.push([req.method, url.pathname]);
        res.setHeader('Cache-Control','no-store');
        if (url.pathname === '/offline-worker.js' || url.pathname === '/offline-catalog.js') {
            res.setHeader('Content-Type','text/javascript');
            return res.end(fs.readFileSync(path.join(player,url.pathname.slice(1))));
        }
        if (url.pathname === '/api/hanging') { hangingRequest(); return; }
        if (unavailable) { res.statusCode = 503; return res.end(); }
        if (authOutage && url.pathname === '/api/player/me') { res.statusCode = 401; return res.end(); }
        if (authOutage && url.pathname === '/offline-probe') { res.statusCode = 500; return res.end(); }
        if (url.pathname === '/logout') { account = ''; return res.end('logged out'); }
        if (url.pathname === '/offline-probe') {
            res.setHeader('X-Furumusic-Player-Account',account || '0');
            res.statusCode = account ? 204 : 401;
            return res.end();
        }
        if (url.pathname === '/') {
            res.setHeader('Content-Type','text/html');
            if (!account) return res.end('<title>Login</title>');
            res.setHeader('X-Furumusic-Player-Account',account);
            return res.end('<html data-offline-mode="online" data-offline-account="'+account+'"><title>Player</title><script>'+catalog+'</script></html>');
        }
        if (url.pathname === '/api/player/me') {
            res.setHeader('Content-Type','application/json');
            return res.end(JSON.stringify({id:Number(account),name:'Network'}));
        }
        return res.end('public');
    });
    await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
    const base = 'http://127.0.0.1:'+server.address().port;
    const browser = await chromium.launch({headless:true,...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {})});
    try {
        const context = await browser.newContext();
        const page = await context.newPage();
        await page.goto(base);
        await page.evaluate(async () => {
            await navigator.serviceWorker.register('/offline-worker.js');
            await navigator.serviceWorker.ready;
        });
        await page.waitForFunction(() => !!navigator.serviceWorker.controller);
        const mode = async (value, id='17') => {
            const result = await page.evaluate(([value,id]) => new Promise(resolve => {
                const ports = new MessageChannel();
                ports.port1.onmessage = ({data}) => resolve(data);
                navigator.serviceWorker.controller.postMessage({type:'offline-config',account:id,mode:value},[ports.port2]);
            }),[value,id]);
            assert.equal(result.ok,true,result.error);
        };
        await mode('online');
        await page.evaluate(async () => {
            const repo = new OfflineCatalog(17);
            await repo.observe('/api/player/me',{id:17,name:'Saved'});
            const file = await (await repo.directory()).getFileHandle('audio.test',{create:true});
            const stream = await file.createWritable();
            await stream.write('0123456789');
            await stream.close();
            await repo.write(['tracks','files'], get => {
                const fileId = 'b3:' + 'a'.repeat(64);
                get('tracks').put({id:42,file:fileId,track:{id:42,title:'Saved track'}});
                get('files').put({name:'audio.test',mime:'audio/mpeg'},fileId);
            });
        });
        const audio = (p,id) => p.evaluate(async id => {
            const response = await fetch('/api/player/stream/'+id+'?saved=1',{headers:{Range:'bytes=1-3'}});
            return [response.status,await response.text()];
        }, id);
        assert.deepEqual(await audio(page,42),[206,'123']);
        await page.evaluate(() => fetch('/login',{method:'POST'}));
        assert.deepEqual(await audio(page,42),[206,'123'],'same-account sign-in keeps the cache');
        assert.deepEqual(await audio(page,43),[200,'public']);
        assert.deepEqual(await page.evaluate(async () => {
            const response = await fetch('/api/player/federation/cache/'+ 'a'.repeat(64) +'?saved=1', {headers:{Range:'bytes=2-4'}});
            return [response.status,await response.text()];
        }),[206,'234']);
        const guest = await context.newPage();
        await guest.goto(base+'/share/token');
        assert.deepEqual(await audio(guest,42),[200,'public'],'guests cannot read member audio');
        await guest.close();
        const started = new Promise(resolve => { hangingRequest = resolve; });
        await page.evaluate(() => { window.old = fetch('/api/hanging').then(r=>r.status).catch(()=>0); });
        await started;
        await mode('manual');
        assert.ok([0,503].includes(await page.evaluate(() => window.old)));
        const baseline = log.length;
        const other = await page.context().newPage();
        await other.goto(base+'/#artists');
        assert.equal(await other.evaluate(() => document.documentElement.dataset.offlineMode),'manual');
        assert.equal(await other.evaluate(() => fetch('/api/player/me').then(r=>r.json()).then(p=>p.name)), 'Saved');
        for (const [url,method] of [['/offline-probe','GET'],['/api/player/history','POST'],['/logout','GET']]) {
            assert.equal(await page.evaluate(([u,m]) => fetch(u,{method:m}).then(r=>r.status),[url, method]),503);
        }
        assert.equal(await page.evaluate(() => fetch(location.origin.replace('127.0.0.1','localhost')+'/external-art',
            {mode:'no-cors'}).then(()=>true,()=>false)),false);
        assert.deepEqual(log.slice(baseline).filter(([,p]) => !['/offline-worker.js','/offline-catalog.js'].includes(p)),[]);

        await mode('auto');
        assert.equal(await page.evaluate(() => fetch('/offline-probe').then(r=>r.status)),204);
        await mode('online');
        assert.equal(await page.evaluate(() => fetch('/api/player/me').then(r=>r.json()).then(p=>p.name)), 'Network');
        authOutage = true;
        assert.equal(await page.evaluate(() => fetch('/api/player/me').then(r=>r.json()).then(p=>p.name)), 'Saved',
            'an API 401 with an unavailable session store keeps local data');
        assert.equal(await page.evaluate(() => fetch('/offline-probe').then(r=>r.status)),503);
        authOutage = false;
        await mode('online');
        unavailable = true;
        assert.deepEqual(await page.evaluate(async () => {
            const response = await fetch('/api/player/tracks-by-ids', {method:'POST',body:JSON.stringify({ids:[42]})});
            return response.json();
        }), [{id:42,title:'Saved track'}]);
        await page.reload();
        assert.equal(await page.evaluate(() => document.documentElement.dataset.offlineMode),'auto');
        assert.equal(await page.evaluate(() => fetch('/api/player/me').then(r=>r.json()).then(p=>p.name)), 'Saved');
        unavailable = false;
        account = '19';
        const probe = await page.evaluate(() => fetch('/offline-probe').then(async r=>[r.status,r.headers.get('X-Furumusic-Player-Account'),await r.text()]));
        assert.deepEqual(probe,[204,'19','']);
        assert.equal(await page.evaluate(() => new OfflineCatalog(17).read('meta','me')),undefined);
        await page.reload();
        assert.equal(await page.evaluate(() => document.documentElement.dataset.offlineAccount),'19');
        await mode('online','19');
        assert.deepEqual(await audio(other,42),[503,'{"error":"Unavailable offline"}'],'an old open tab cannot use the new account');
        await other.close();
        await mode('disabled','19');
        await page.evaluate(() => fetch('/logout'));
        await page.reload();
        assert.equal(await page.title(),'Login');
        console.log('Offline worker passed cold navigation, network isolation, automatic fallback and account/guest isolation.');
    } finally {
        await browser.close();
        server.closeAllConnections();
        await new Promise(resolve => server.close(resolve));
    }
})().catch(error => { console.error(error); process.exitCode=1; });

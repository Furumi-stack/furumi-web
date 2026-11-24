// Run against a disposable, freshly migrated instance only.
// Requires Docker PostgreSQL (FURU_ACCESS_TEST_PG), Playwright and Chrome.
const assert = require('node:assert/strict');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const {spawnSync} = require('node:child_process');
const {mkdirSync, writeFileSync} = require('node:fs');
const {createServer} = require('node:http');
const {generateKeyPairSync, sign, randomUUID, createHash} = require('node:crypto');
const base = process.env.FURU_TEST_URL || 'http://127.0.0.1:18048';
const issuer = 'http://127.0.0.1:18049';
const mediaRoot = process.env.FURU_ACCESS_TEST_MEDIA || '/tmp/furumi-access-media';
function sql(statement) {
    const result = spawnSync('docker', ['exec', '-i', process.env.FURU_ACCESS_TEST_PG || 'furumi-access-test-pg', 'psql', '-U', 'postgres', '-d', 'furumi_access_test', '-At', '-v', 'ON_ERROR_STOP=1'], {input:statement, encoding:'utf8'});
    assert.equal(result.status, 0, result.stderr); return result.stdout.trim();
}
const encode = value => Buffer.from(JSON.stringify(value)).toString('base64url');
const {privateKey, publicKey} = generateKeyPairSync('rsa', {modulusLength:2048});
const jwk = {...publicKey.export({format:'jwk'}), kid:'test', use:'sig', alg:'RS256'};
let claims = {sub:'pending-test', email:'pending@example.test', name:'Pending Listener', groups:[]};
const codes = new Map();
const oidc = createServer(async (request, response) => {
    const url = new URL(request.url, issuer);
    let data;
    if (url.pathname === '/.well-known/openid-configuration') {
        data = {issuer, authorization_endpoint:issuer+'/authorize', token_endpoint:issuer+'/token', jwks_uri:issuer+'/jwks', response_types_supported:['code'], subject_types_supported:['public'], id_token_signing_alg_values_supported:['RS256'], token_endpoint_auth_methods_supported:['client_secret_basic'], scopes_supported:['openid','email','profile'], claims_supported:['sub','email','email_verified','name','groups']};
    } else if (url.pathname === '/jwks') data = {keys:[jwk]};
    else if (url.pathname === '/authorize') {
        const code = randomUUID(); codes.set(code, {...claims, nonce:url.searchParams.get('nonce'), challenge:url.searchParams.get('code_challenge')});
        const target = new URL(url.searchParams.get('redirect_uri'));
        target.searchParams.set('code', code); target.searchParams.set('state', url.searchParams.get('state'));
        response.writeHead(302, {Location:target.toString()}); response.end(); return;
    } else if (url.pathname === '/token') {
        let body=''; for await (const chunk of request) body += chunk;
        const form = new URLSearchParams(body); const saved = codes.get(form.get('code')); codes.delete(form.get('code'));
        assert.ok(saved); assert.equal(createHash('sha256').update(form.get('code_verifier')).digest('base64url'), saved.challenge);
        const {challenge, ...identity} = saved;
        const payload = {iss:issuer, aud:'furumi-test', iat:Math.floor(Date.now()/1000), exp:Math.floor(Date.now()/1000)+300, email_verified:true, ...identity};
        const token = encode({alg:'RS256',kid:'test'})+'.'+encode(payload);
        data = {access_token:'test-access', token_type:'Bearer', expires_in:300, id_token:token+'.'+sign('RSA-SHA256',Buffer.from(token),privateKey).toString('base64url')};
    } else { response.writeHead(404); response.end(); return; }
    response.writeHead(200, {'Content-Type':'application/json'}); response.end(JSON.stringify(data));
});
(async () => {
    await new Promise(resolve => oidc.listen(18049,'127.0.0.1',resolve));
    const browser = await chromium.launch({headless:true, executablePath:process.env.CHROME_PATH || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'});
    const errors=[];
    try {
        const admin = await browser.newContext({viewport:{width:1440,height:1000}});
        let response = await admin.request.post(base+'/admin/setup', {form:{username:'access-admin',password:'access-test-password',confirm_password:'access-test-password'}});
        assert.ok(response.ok());
        response = await admin.request.post(base+'/login',{form:{username:'access-admin',password:'access-test-password'}}); assert.ok(response.ok());
        sql(`INSERT INTO furumusic__config_entry (key,value) VALUES ('auth_sso_enabled','true'),('oidc_issuer','${issuer}'),('oidc_client_id','furumi-test'),('oidc_client_secret','test-${randomUUID()}'),('oidc_admin_groups','admins'),('oidc_user_groups','listeners') ON CONFLICT (key) DO UPDATE SET value=EXCLUDED.value;`);
        sql("UPDATE furumusic__user SET local_role=NULL, sso_role=NULL, is_active=false WHERE username IN ('pending','automatic','root-new');");
        mkdirSync(mediaRoot,{recursive:true});
        const wav=Buffer.alloc(44+8000*2*10); wav.write('RIFF'); wav.writeUInt32LE(wav.length-8,4); wav.write('WAVEfmt ',8); wav.writeUInt32LE(16,16); wav.writeUInt16LE(1,20); wav.writeUInt16LE(1,22); wav.writeUInt32LE(8000,24); wav.writeUInt32LE(16000,28); wav.writeUInt16LE(2,32); wav.writeUInt16LE(16,34); wav.write('data',36); wav.writeUInt32LE(wav.length-44,40);
        writeFileSync(mediaRoot+'/shared-test.wav',wav);
        const cover=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aPioAAAAASUVORK5CYII=','base64'); writeFileSync(mediaRoot+'/cover.png',cover);
        sql(`INSERT INTO furumusic__media_file (id,file_type,file_path,original_filename,mime_type,file_size_bytes,sha256_hash,uploader_name,created_at) VALUES (9001,'audio','shared-test.wav','Shared test.wav','audio/wav',${wav.length},'${createHash('sha256').update(wav).digest('hex')}','Test','2026-11-18T00:00:00Z'),(9002,'cover_art','cover.png','cover.png','image/png',${cover.length},'${createHash('sha256').update(cover).digest('hex')}','Test','2026-11-18T00:00:00Z') ON CONFLICT (id) DO NOTHING;
        INSERT INTO furumusic__artist (id,name,name_sort,is_hidden,created_at,updated_at) VALUES (9001,'Test Artist','test artist',false,'2026-11-18T00:00:00Z','2026-11-18T00:00:00Z') ON CONFLICT (id) DO NOTHING;
        INSERT INTO furumusic__release (id,title,title_sort,release_type,is_hidden,cover_file_id,created_at,updated_at) VALUES (9001,'Shared Album','shared album','album',false,9002,'2026-11-18T00:00:00Z','2026-11-18T00:00:00Z') ON CONFLICT (id) DO NOTHING;
        INSERT INTO furumusic__track (id,title,title_sort,release_id,duration_seconds,audio_file_id,is_hidden,created_at,updated_at) VALUES (9001,'Shared Song','shared song',9001,10,9001,false,'2026-11-18T00:00:00Z','2026-11-18T00:00:00Z'),(9002,'Private Song','private song',9001,10,9001,false,'2026-11-18T00:00:00Z','2026-11-18T00:00:00Z') ON CONFLICT (id) DO NOTHING;
        INSERT INTO furumusic__track_artist (track_id,artist_id,role,position) SELECT id,9001,'main',0 FROM furumusic__track WHERE id IN (9001,9002) AND NOT EXISTS (SELECT 1 FROM furumusic__track_artist WHERE track_id=furumusic__track.id);`);
        response=await admin.request.post(base+'/api/player/shares',{data:{track_ids:[9001],title:'Shared Song'}}); assert.equal(response.status(),200); const share=await response.json();
        const guest=await browser.newContext({viewport:{width:1440,height:1000}});
        const page=await guest.newPage(); page.on('pageerror',e=>errors.push(e.message));
        const api=[]; page.on('request',r=>{if(r.url().includes('/api/'))api.push(new URL(r.url()).pathname);});
        await page.goto(base+share.url); await page.waitForSelector('.playlist-track-row');
        assert.equal(await page.locator('.playlist-track-row').count(),1);
        assert.equal(await page.getByRole('link',{name:'Sign in / register'}).count(),1);
        assert.equal(await page.locator('#search-input').isVisible(),false);
        assert.equal(await page.locator('.sidebar-left').count(),0);
        await page.locator('.player-btn-play').click(); await page.waitForFunction(()=>Alpine.store('player').currentTime>0);
        assert.equal(await page.locator('.device-picker').isVisible(),false);
        const more=page.locator('.playlist-track-row .track-more-btn'),menu=page.locator('.track-action-menu:visible');
        await more.click();
        assert.equal(await menu.locator('[data-action="similar"]:enabled:visible').count(),0);
        await menu.locator('[data-action="info"]').click();
        const infoModal=page.locator('.info-modal');
        await infoModal.waitFor();
        assert.equal(await infoModal.getByText('Uploader',{exact:true}).count(),0);
        for (const viewer of [guest, admin]) {
            const detail = await viewer.request.get(base+'/api/player/shares/'+share.token);
            assert.equal((await detail.json()).tracks[0].uploader_name, '');
        }
        assert.equal(await infoModal.getByRole('button',{name:/start radio|find similar|download/i}).count(),0);
        await infoModal.locator('[title="Close"]').click();
        await more.click();
        const downloading=page.waitForEvent('download'); await menu.locator('[data-action="download"]').click(); const download=await downloading;
        assert.equal(download.suggestedFilename(),'Shared test.wav'); await download.saveAs('/tmp/furumi-access-download.wav');
        await page.screenshot({path:'/tmp/furumi-guest-desktop.png',fullPage:true});
        assert.ok(api.every(path=>path.startsWith('/api/player/shares/')),JSON.stringify(api));
        for (const action of ['stream','download','cover']) {
            assert.equal((await guest.request.get(`${base}/api/player/shares/${share.token}/${action}/9002`)).status(),404);
            assert.equal((await guest.request.get(`${base}/api/player/shares/${'0'.repeat(32)}/${action}/9001`)).status(),404);
        }
        assert.equal((await guest.request.get(`${base}/api/player/stream/9001`)).status(),401);
        assert.equal((await guest.request.get(`${base}/api/player/artists`)).status(),401);
        response=await guest.request.get(`${base}/api/player/shares/${share.token}/stream/9001`,{headers:{Range:'bytes=0-43'}}); assert.equal(response.status(),206); assert.equal((await response.body()).length,44);
        assert.equal((await guest.request.get(`${base}/api/player/shares/${share.token}/stream/9001`,{headers:{Range:'bytes=999999999-'}})).status(),416);
        assert.equal((await guest.request.get(`${base}/api/player/shares/${share.token}/cover/9001`)).status(),200);
        assert.equal((await guest.request.post(`${base}/api/player/shares`,{data:{track_ids:[9001]}})).status(),401);
        await page.getByRole('link',{name:'Sign in / register'}).click(); await page.locator('.sso-btn').click();
        await page.waitForURL(base+share.url); await page.getByText('Awaiting approval',{exact:true}).waitFor(); await page.waitForSelector('.playlist-track-row');
        assert.equal(await page.locator('.playlist-track-row').count(),1);
        const pendingId=Number(sql("SELECT id FROM furumusic__user WHERE username='pending'")); assert.ok(pendingId);
        await page.goto(base+'/'); await page.getByRole('heading',{name:'Awaiting approval'}).waitFor();
        assert.equal(await page.locator('.app-layout').count(),0);
        assert.equal((await guest.request.get(`${base}/api/player/artists`)).status(),401);
        assert.equal((await guest.request.post(`${base}/admin/v2/api/users/manage`,{data:{action:'approve',id:pendingId,role:'admin'}})).status(),401);
        const adminPage=await admin.newPage(); adminPage.on('pageerror',e=>errors.push(e.message));
        await adminPage.goto(base+'/admin/#users');
        const pendingRow=adminPage.locator('.users-table tr').filter({hasText:'Pending Listener'});
        await pendingRow.getByRole('button',{name:'Approve as user',exact:true}).click();
        await page.waitForSelector('.app-layout',{timeout:15000});
        assert.equal((await guest.request.get(`${base}/api/player/artists`)).status(),200);
        assert.equal(sql(`SELECT local_role || ':' || role FROM furumusic__user WHERE id=${pendingId}`),'user:user');
        // Priority: a user SSO group overrides even a local admin grant; losing
        // that group restores the separate local grant.
        response=await admin.request.post(base+'/admin/v2/api/users/manage',{data:{action:'approve',id:pendingId,role:'admin'}}); assert.equal(response.status(),200);
        claims.groups=['listeners']; await page.goto(base+'/logout'); await page.locator('.sso-btn').click(); await page.waitForURL(base+'/');
        assert.equal(sql(`SELECT local_role || ':' || role FROM furumusic__user WHERE id=${pendingId}`),'admin:user');
        claims.groups=[]; await page.goto(base+'/logout'); await page.locator('.sso-btn').click(); await page.waitForURL(base+'/');
        assert.equal(sql(`SELECT local_role || ':' || role FROM furumusic__user WHERE id=${pendingId}`),'admin:admin');
        claims={sub:'automatic-test',email:'automatic@example.test',name:'Automatic Listener',groups:['admins']};
        await page.goto(base+'/logout'); await page.locator('.sso-btn').click(); await page.waitForURL(base+'/');
        assert.equal(sql("SELECT COALESCE(local_role,'none') || ':' || role FROM furumusic__user WHERE username='automatic'"),'none:admin');
        const automaticId=Number(sql("SELECT id FROM furumusic__user WHERE username='automatic'"));
        response=await admin.request.post(base+'/admin/v2/api/users/manage',{data:{action:'update',id:automaticId,username:'automatic',display_name:'Renamed Automatic Listener',email:'automatic@example.test',role:''}});
        assert.equal(response.status(),200);
        assert.equal(sql("SELECT COALESCE(local_role,'none') || ':' || role FROM furumusic__user WHERE username='automatic'"),'none:admin');
        claims.groups=[]; await page.goto(base+'/logout'); await page.locator('.sso-btn').click(); await page.getByRole('heading',{name:'Awaiting approval'}).waitFor();
        assert.equal(sql("SELECT is_active FROM furumusic__user WHERE username='automatic'"),'f');
        // A completely new identity arriving at the root never sees a demo player.
        claims={sub:'new-root-test',email:'root-new@example.test',name:'New Root Listener',groups:[]};
        await page.goto(base+'/logout'); await page.locator('.sso-btn').click(); await page.getByRole('heading',{name:'Awaiting approval'}).waitFor();
        assert.equal(await page.locator('.app-layout').count(),0);
        // Queue snapshots preserve order and repeated tracks.
        response=await admin.request.post(base+'/api/player/shares',{data:{track_ids:[9002,9001,9002],title:'Shared queue'}}); const queue=await response.json();
        response=await guest.request.get(base+'/api/player/shares/'+queue.token); assert.deepEqual((await response.json()).tracks.map(t=>t.id),[9002,9001,9002]);
        const mobile=await browser.newContext({viewport:{width:390,height:844},isMobile:true,hasTouch:true}); const mobilePage=await mobile.newPage(); mobilePage.on('pageerror',e=>errors.push(e.message));
        await mobilePage.goto(base+queue.url); await mobilePage.waitForSelector('.playlist-track-row');
        assert.equal(await mobilePage.getByRole('link',{name:'Sign in / register'}).isVisible(),true);
        const mobileMore=mobilePage.locator('.playlist-track-row .track-more-btn').first();
        const bounds=await mobileMore.boundingBox(); assert.ok(bounds && bounds.x>=0 && bounds.x+bounds.width<=390,JSON.stringify(bounds));
        await mobileMore.tap();
        const mobileMenu=mobilePage.locator('.track-action-menu:visible');
        await mobileMenu.locator('[data-action="download"]').waitFor();
        await mobileMenu.locator('[data-action="info"]').tap();
        await mobilePage.locator('.info-modal [title="Close"]').tap();
        await mobilePage.screenshot({path:'/tmp/furumi-guest-mobile.png',fullPage:true});
        for (const old of ['/share/track/9001','/share/release/9001','/share/playlist/abc','/api/player/radio/track/9001','/api/player/share-playlist']) assert.equal((await guest.request.get(base+old)).status(),404);
        assert.deepEqual(errors,[]);
        console.log('PASS: OIDC approval, live activation, SSO precedence and removal, anonymous playback/download/cover, scope enforcement, queue snapshots, desktop and mobile, removal of legacy routes.');
    } finally { await browser.close(); await new Promise(resolve=>oidc.close(resolve)); }
})().catch(error=>{console.error(error);process.exitCode=1;});

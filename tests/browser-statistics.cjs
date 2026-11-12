// Run against a fresh disposable instance and the named disposable PostgreSQL container.
// PLAYWRIGHT_MODULE may point to an existing Playwright installation.
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assert = require('node:assert/strict');
const {spawnSync} = require('node:child_process');
const base = process.env.FURU_TEST_URL || 'http://127.0.0.1:3018';
(async () => {
    const browser = await chromium.launch({headless:true, executablePath:process.env.CHROME_PATH || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'});
    try {
        const context = await browser.newContext({viewport:{width:1440,height:1100}, ignoreHTTPSErrors:true});
        const page = await context.newPage(); const errors=[];
        page.on('pageerror', error => errors.push(error.message));
        const anonymous = await context.request.get(`${base}/admin/v2/api/statistics`);
        assert.equal(anonymous.status(),401);
        await page.goto(`${base}/admin/setup`);
        await page.getByLabel('Username', {exact:true}).fill('alice');
        await page.getByLabel('Password', {exact:true}).fill('statistics-test-password');
        await page.locator('[name=confirm_password]').fill('statistics-test-password');
        await page.locator('form button[type=submit]').click();
        await page.waitForURL('**/admin/v2');
        await page.getByRole('button', {name:'Users',exact:true}).click();
        await page.getByRole('button', {name:'Add user',exact:true}).click();
        const editor = page.locator('.modal-backdrop').filter({has:page.getByRole('button',{name:'Save user',exact:true})});
        await editor.getByLabel('username', {exact:true}).fill('bob');
        await editor.getByLabel('password', {exact:true}).fill('statistics-test-password');
        await editor.getByRole('button',{name:'Save user',exact:true}).click();
        await editor.waitFor({state:'hidden'});
        const sql = `INSERT INTO furumusic__listen_event
            (user_id,listen_id,content_id,local_track_id,origin_device_id,started_at_ms,listened_ms,track_duration_ms,ended_reason,qualified,metadata_json,created_at)
            SELECT 1,'ui-'||n,'content-one',NULL,'web-a',1793491200000+n*1000,180000,180000,'"finished"',true,
            '{"title":"First song","artist_names":["Artist A"],"release_title":"Album"}'::jsonb,'2026-11-01T00:00:00Z' FROM generate_series(1,30) n;
            INSERT INTO furumusic__listen_event
            (user_id,listen_id,content_id,local_track_id,origin_device_id,started_at_ms,listened_ms,track_duration_ms,ended_reason,qualified,metadata_json,created_at)
            VALUES (2,'ui-bob','content-one',NULL,'web-b',1793664000000,90000,180000,'"skipped"',true,
            '{"title":"First song","artist_names":["Artist A"],"release_title":"Album"}','2026-11-03T00:00:00Z');`;
        const seeded = spawnSync('docker',['exec','-i','furumi-statistics-test-pg','psql','-U','postgres','-d','furumi_statistics_test','-v','ON_ERROR_STOP=1'],{input:sql, encoding:'utf8'});
        assert.equal(seeded.status,0,seeded.stderr);
        await page.getByRole('button',{name:'Statistics',exact:true}).click();
        const area = page.locator('.content').filter({has:page.getByText('Listening statistics',{exact:true})});
        await area.getByRole('button',{name:'Artist A',exact:true}).first().waitFor();
        const artistResponse = page.waitForResponse(r => r.url().includes('/api/statistics?') && r.url().includes('artist=Artist+A'));
        await area.getByRole('button',{name:'Artist A',exact:true}).first().click();
        assert.equal((await artistResponse).status(),200);
        await area.getByRole('button',{name:'bob',exact:true}).first().waitFor();
        const bobResponse = page.waitForResponse(r => r.url().includes('/api/statistics?') && r.url().includes('user_id=2'));
        await area.getByRole('button',{name:'bob',exact:true}).first().click();
        const bob = await (await bobResponse).json();
        assert.equal(bob.summary.plays,1); assert.equal(bob.history.items[0].user_name,'bob');
        assert.equal(bob.history.items[0].title,'First song');
        const cleared = page.waitForResponse(r => r.url().includes('/api/statistics?') && !r.url().includes('user_id=') && !r.url().includes('artist='));
        await area.getByRole('button',{name:'All listeners and music'}).click();
        await cleared;
        await area.getByText('Loading statistics…',{exact:true}).waitFor({state:'hidden'});
        await page.screenshot({path:'/tmp/furumi-statistics-desktop.png',fullPage:false});
        await page.setViewportSize({width:390,height:844});
        await page.screenshot({path:'/tmp/furumi-statistics-mobile.png',fullPage:false});
        await page.setViewportSize({width:1440,height:1100});
        const usersResponse = await context.request.get(`${base}/admin/v2/api/users`); const users = await usersResponse.json();
        assert.equal(users.items.find(u=>u.username==='bob').plays,1);
        const detailResponse = await context.request.get(`${base}/admin/v2/api/users/2`); const detail = await detailResponse.json();
        assert.equal(detail.stats.plays,1); assert.equal(detail.recent_plays[0].title,'First song');
        const next = await context.request.get(`${base}/admin/v2/api/statistics?from=all&history_page=2`);
        assert.equal((await next.json()).history.items.length,6);
        for (const path of ['/admin/debug','/admin/users','/admin/settings','/admin/artists','/admin/reviews','/admin/jobs']) {
            assert.equal((await context.request.get(base+path)).status(),404,path);
        }
        assert.equal((await context.request.get(`${base}/admin/v2/api/statistics?from=invalid`)).status(),400);
        const jobs = await (await context.request.get(`${base}/admin/v2/api/jobs`)).json();
        const job = jobs.find(j => j.name === 'metadata_backfill');
        if (job.enabled) assert.equal((await context.request.post(`${base}/admin/v2/api/jobs/${job.name}/toggle`)).status(),200);
        assert.equal((await context.request.post(`${base}/admin/v2/api/jobs/${job.name}/schedule`,{data:{cron:'0 17 3 * * *'}})).status(),200);
        await context.request.get(`${base}/admin/v2/api/dashboard`);
        const changedJob = (await (await context.request.get(`${base}/admin/v2/api/jobs`)).json()).find(j=>j.name===job.name);
        assert.equal(changedJob.enabled,false); assert.equal(changedJob.cron_expression,'0 17 3 * * *');
        const ownDelete = await context.request.post(`${base}/admin/v2/api/users/manage`, {data:{action:'delete',id:1}});
        assert.equal(ownDelete.status(),400);
        const listenerContext = await browser.newContext({ignoreHTTPSErrors:true});
        const login = await listenerContext.newPage();
        await login.goto(`${base}/login`);
        await login.locator('[name=username]').fill('bob');
        await login.locator('[name=password]').fill('statistics-test-password');
        await login.locator('form button[type=submit]').click();
        await login.waitForURL(base + '/');
        assert.equal((await listenerContext.request.get(`${base}/admin/v2/api/statistics`)).status(),403);
        await listenerContext.close();
        assert.deepEqual(errors,[]);
        console.log('PASS: setup, user creation, artist → listeners → user history, counts, pagination, invalid filters, v1 removal, desktop/mobile rendering');
    } finally { await browser.close(); }
})().catch(error => {console.error(error); process.exitCode=1});

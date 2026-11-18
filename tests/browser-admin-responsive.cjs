// Run against a disposable local instance with at least one admin and one release.
// Existing data is read only; long-content fixtures are injected into browser state.
// FURU_TEST_URL, FURU_TEST_USERNAME, FURU_TEST_PASSWORD, PLAYWRIGHT_MODULE,
// CHROME_PATH and FURU_TEST_SCREENSHOTS configure the runner.
const assert = require('node:assert/strict');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const {mkdirSync} = require('node:fs');
const base = process.env.FURU_TEST_URL || 'http://127.0.0.1:18048';
(async () => {
    const browser = await chromium.launch({headless:true, executablePath:process.env.CHROME_PATH});
    try {
        const context = await browser.newContext({viewport:{width:390,height:844}, reducedMotion:'reduce'});
        const response = await context.request.post(base+'/login', {form:{username:process.env.FURU_TEST_USERNAME || 'access-admin', password:process.env.FURU_TEST_PASSWORD || 'access-test-password'}});
        assert.ok(response.ok());
        const page = await context.newPage();
        const errors = []; page.on('pageerror', error => errors.push(error.message));
        await page.goto(base+'/admin/v2');
        await page.waitForFunction(() => window.Alpine && document.querySelector('.admin-shell')._x_dataStack);
        const state = (fn, arg) => page.evaluate(({code,arg}) => Function('app','arg',`return (${code})(app,arg)`)(Alpine.$data(document.querySelector('.admin-shell')), arg), {code:fn.toString(),arg});
        await state(async app => { clearInterval(app.poller); await app.refreshAll(); });
        const screenshot = async name => {
            if (!process.env.FURU_TEST_SCREENSHOTS) return;
            mkdirSync(process.env.FURU_TEST_SCREENSHOTS, {recursive:true});
            await page.screenshot({path:process.env.FURU_TEST_SCREENSHOTS+'/'+name+'.png'});
        };
        async function fits(label) {
            const overflow = await page.evaluate(() => [...document.querySelectorAll('html, body, main, .content, .panel, .modal, .modal-body')]
                .filter(el => el.getClientRects().length && el.scrollWidth > el.clientWidth + 2)
                .map(el => `${el.tagName}.${el.className}: ${el.scrollWidth} > ${el.clientWidth}`));
            assert.deepEqual(overflow, [], label);
        }
        async function settle() { await page.waitForTimeout(200); }
        async function navigate(view) {
            if (await page.locator('.menu-toggle').isVisible()) await page.locator('.menu-toggle').click();
            await page.locator('.nav-btn').filter({hasText:{reviews:'Review Queue',jobs:'Tasks',library:'Library Workbench',tools:'Library Tools',statistics:'Statistics',users:'Users',settings:'Settings'}[view]}).click();
            await page.waitForFunction(v => Alpine.$data(document.querySelector('.admin-shell')).activeView === v, view);
            await settle();
            assert.equal(await page.locator('main').evaluate(el => el.inert), false);
        }
        // Navigate through the actual menu, testing both permanent and drawer modes.
        for (const width of [320,390,768,1024,1440]) {
            await page.setViewportSize({width,height:844}); await settle();
            for (const view of ['reviews','jobs','library','tools','statistics','users','settings']) {
                await navigate(view); await fits(`${width}: ${view}`);
                if (width === 390 || width === 1440) await screenshot(`${width}-${view}`);
            }
        }
        await page.setViewportSize({width:390,height:844}); await settle();
        await page.locator('.menu-toggle').click(); await settle();
        assert.equal(await page.locator('main').evaluate(el => el.inert), true);
        await screenshot('390-navigation');
        await page.keyboard.press('Shift+Tab');
        assert.equal(await page.evaluate(() => document.activeElement.classList.contains('nav-player')), true);
        await page.keyboard.press('Tab');
        assert.equal(await page.evaluate(() => document.activeElement.hasAttribute('data-dialog-close')), true);
        await page.keyboard.press('Escape'); await settle();
        assert.equal(await page.evaluate(() => document.activeElement.classList.contains('menu-toggle')), true);
        await page.locator('.menu-toggle').click(); await page.setViewportSize({width:1440,height:844}); await settle();
        assert.equal(await page.locator('main').evaluate(el => el.inert), false);
        assert.equal(await page.locator('.sidebar').evaluate(el => el.inert), false);
        await page.setViewportSize({width:390,height:844}); await settle();
        await navigate('jobs');
        await page.locator('#task-pane-list tbody tr').first().click(); await settle();
        assert.equal(await page.locator('#task-pane-list').isVisible(), false);
        assert.equal(await page.locator('#task-pane-details').isVisible(), true);
        await page.getByRole('button', {name:'Edit schedule',exact:true}).click();
        await page.getByRole('textbox', {name:'Cron schedule'}).fill('0 15 * * * *');
        await page.locator('#task-tab-logs').click();
        await page.keyboard.press('ArrowLeft');
        assert.equal(await page.getByRole('textbox', {name:'Cron schedule'}).inputValue(), '0 15 * * * *');
        await fits('task editor');
        await navigate('settings');
        await page.locator('.section-jump select').selectOption('federation'); await settle();
        assert.equal(await page.evaluate(() => document.activeElement.id), 'settings-federation');
        await page.getByRole('button',{name:'Save settings',exact:true}).scrollIntoViewIfNeeded();
        await fits('settings footer');
        // Shared dialog lifecycle: trap focus, lock background, retain close affordance,
        // scroll long content and restore focus to its trigger.
        async function checkDialog(name) {
            const dialog = page.locator('.modal:visible'); await dialog.waitFor(); await settle();
            assert.equal(await page.locator('main').evaluate(el => el.inert), true);
            await fits(name);
            assert.ok((await dialog.locator('.modal-body').innerText()).trim().length > 0, name+' body populated');
            const box = await dialog.boundingBox();
            if (page.viewportSize().width < 768) assert.ok(Math.abs(box.height-page.viewportSize().height)<2, name+' full height');
            await dialog.focus(); await page.keyboard.press('Shift+Tab');
            assert.equal(await dialog.evaluate(el => el.contains(document.activeElement)), true);
            await page.keyboard.press('Tab');
            assert.equal(await page.evaluate(() => document.activeElement.hasAttribute('data-dialog-close')), true);
            await screenshot(name);
            await dialog.locator('.modal-body').evaluate(el => { el.scrollTop=el.scrollHeight; });
            await fits(name+' bottom');
            const close = await dialog.locator('[data-dialog-close]').boundingBox();
            assert.ok(close.y >= 0 && close.y+close.height <= page.viewportSize().height, name+' close reachable');
            const footer = dialog.locator('.dialog-actions:visible');
            if (await footer.count()) {
                const bottom = await footer.boundingBox();
                assert.ok(bottom.y >= -1 && bottom.y+bottom.height <= page.viewportSize().height+1, name+' actions reachable');
            }
            await page.keyboard.press('Escape'); await settle();
            assert.equal(await page.locator('.modal:visible').count(),0);
            assert.equal(await page.locator('main').evaluate(el => el.inert),false);
        }
        await navigate('users');
        await page.getByRole('button',{name:'Add user',exact:true}).click();
        await checkDialog('390-add-user');
        assert.equal(await page.evaluate(() => document.activeElement.textContent.trim()), 'Add user');
        await page.locator('.users-table tbody tr').first().click();
        await checkDialog('390-user-profile');
        const review = {id:987654,status:'pending',review_type:'metadata',filename:'Very long track title '.repeat(8),display_path:'/inbox/'+'album/'.repeat(20),tags:[],normalized:{title:'Long song title',artist:'Test artist',notes:'Notes\n'.repeat(40)}};
        await navigate('reviews');
        await state((app,row) => {app.reviews.items=[row]; app.reviews.total=1; app.openReview(row);}, review);
        await checkDialog('390-review');
        await fits('populated reviews');
        await navigate('library');
        await state(async app => {app.libraryKind='releases'; await app.loadLibrary(); app.openEditor(app.library.items[0]);});
        await page.waitForFunction(() => !Alpine.$data(document.querySelector('.admin-shell')).editorLoading);
        await state(app => {app.editorDraft.release_tracks = Array.from({length:12}, (_,i)=>({...app.editorDraft.release_tracks[0],id:700000+i,title:'Very long track title '.repeat(8),disc_number:1,track_number:i+1}));});
        await page.locator('.modal:visible input[x-model="editorDraft.title"]').waitFor({state:'visible'});
        assert.equal(await page.locator('.modal:visible .release-track-row:visible').count(),12);
        await checkDialog('390-release-editor');
        await state(app => {
            const detail = app.editorDetail;
            app.mergeDetails=[detail,{...detail,item:{...detail.item,id:700001},title:'Second release'}];
            app.mergeDraft={release_ids:[detail.item.id,700001],target_release_id:detail.item.id,title:detail.title,release_type:'album',hidden:'false',year:'2026',artist_ids:[],cover_file_id:null,tracks:app.editorDraft.release_tracks};
            app.mergeOpen=true;
        });
        await checkDialog('390-merge');
        // Short landscape viewport and desktop dialog geometry use the same contract.
        for (const viewport of [{width:320,height:568},{width:844,height:390},{width:1440,height:900}]) {
            await page.setViewportSize(viewport); await settle();
            await state((app,row)=>app.openReview(row),review);
            await checkDialog(`${viewport.width}-review`);
        }
        await page.setViewportSize({width:320,height:700}); await settle();
        await navigate('statistics');
        await state(async app => {
            await app.loadStatistics();
            app.statisticsData.ranking.items=[{key:'fixture',label:'Very long artist name '.repeat(12),detail:'long_identifier_'.repeat(20),plays:42,listeners:10,listened_seconds:10000,last_play_ms:Date.now()}];
            app.statisticsData.history.items=[{user_id:1,listen_id:'fixture',user_name:'Listener '.repeat(12),title:'Long song name '.repeat(12),content_id:'fixture',artist_names:['Artist '.repeat(20)],release_title:'Album '.repeat(20),started_at_ms:Date.now(),listened_ms:60000,track_duration_ms:90000,qualified:true,ended_reason:'ended',device_name:'LongDeviceName'.repeat(20),source:'instance'}];
        });
        await settle(); await fits('320 populated statistics');
        await navigate('tools');
        await state(app => {
            app.repairExpanded={snapshot:{artists:[{id:1,name:'Artist'}],releases:[{id:2,title:'Album',year:2026,release_type:'album',artist_ids:[1]}],tracks:[]}};
            app.repairPlan={artists:[],releases:[{id:2,title:'Album '.repeat(30),year:2026,release_type:'album',artist_ids:[1]}],tracks:[],questions:[],delete:{artists:[],releases:[],tracks:[]}};
            app.repairPlanOpen=true;
        });
        await page.locator('.repair-manual summary').click(); await settle();
        await fits('320 manual metadata comparison');
        const comparison=page.locator('.repair-manual');
        assert.ok(await comparison.evaluate(el=>el.scrollWidth>el.clientWidth), 'wide comparison scrolls within its own region');
        await screenshot('320-manual-comparison');
        await navigate('users');
        await page.locator('.users-table tbody tr').first().click();
        await page.getByRole('button',{name:'Edit user',exact:true}).click();
        await checkDialog('320-edit-user');
        await page.locator('.menu-toggle').click();
        await page.goBack(); await settle();
        assert.equal(await page.locator('main').evaluate(el=>el.inert),false, 'browser Back closes navigation');
        assert.deepEqual(errors, []);
        console.log('PASS: 7 sections × 5 widths; drawer, task tabs, settings jump and all 5 dialogs, keyboard and long content.');
    } finally { await browser.close(); }
})().catch(error => {console.error(error); process.exitCode=1;});

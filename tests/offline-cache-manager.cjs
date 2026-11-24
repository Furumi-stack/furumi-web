// Exercises the settings dialog against real records and OPFS files in the offline player test.
const assert = require('node:assert/strict');

async function seedCache(page) {
    await page.evaluate(async () => {
        const repo = new OfflineCatalog(document.documentElement.dataset.offlineAccount);
        await repo.clear();
        const band = {id:7, name:'Cache test artist'};
        const release = {id:9, title:'Cache test release', artists:[band]};
        for (let id = 1; id <= 52; id++) {
            // An edition retained in the cache shares its recording with the first track.
            const file = id === 4 ? 1 : id;
            const bytes = file * 1000;
            const track = {id, title:'Cache track ' + String(id).padStart(2,'0'),
                content_id:'b3:' + file.toString(16).padStart(64,'0'), artists:[band], featured_artists:[],
                release_id:release.id, release_title:release.title, stream_url:'/api/player/stream/' + id,
                audio_format:'MP3', audio_bitrate:id === 9 ? 320 : 128,
                has_lyrics:false, file_size_bytes:bytes};
            await repo.save(track, {release, fetch:async url => url === track.stream_url
                ? new Response(new Uint8Array(bytes), {headers:{'Content-Type':'audio/mpeg','Content-Length':String(bytes)}})
                : new Response(null, {status:404})});
        }
        const rows = await repo.all('tracks');
        const midnight = day => new Date(day + 'T00:00:00').getTime();
        await repo.write(['tracks','plays'], get => {
            for (const row of rows) {
                const {id} = row;
                const day = id <= 3 ? '2023-01-01' : id === 4 ? '2023-01-10' :
                    id <= 7 ? '2024-01-01' : id === 8 ? '2024-01-10' : '2025-01-01';
                row.downloaded_at = midnight(day) + (id === 52 ? 1000 : 0);
                get('tracks').put(row);
                // #5 has never been played. #6 was last played before the cutoff.
                // #4 is a recent listen despite its old download date.
                if (id !== 5) get('plays').put({id, play_count:id === 7 ? 500 : 1,
                    last_played_at:midnight(id === 6 ? '2024-01-03' : id === 17 ? '2025-02-01' : '2025-01-02')}, id);
            }
        });
        await Alpine.store('offline').refresh();
    });
}

async function checkCacheManager(page, networkLog, width) {
    await page.setViewportSize({width,height:width < 600 ? 740 : 900});
    await seedCache(page);
    const baseline = networkLog.length;
    const mobile = width < 600;
    const trigger = page.locator(mobile ? '.mobile-account-chip' : '.sidebar-account-trigger');
    const account = page.locator(mobile ? '#mobile-account-actions' : '#sidebar-account-actions');
    await trigger.click();
    await account.getByRole('button',{name:'Manage offline music'}).click();
    const settings = page.locator('dialog:modal .offline-settings');
    await settings.getByText('Saved music',{exact:true}).waitFor();
    assert.equal(await settings.getByText(/^Cache track/).count(), 0, 'saved music is a summary in settings');
    await settings.evaluate(el => { window.settingsDialog = el.closest('dialog'); });
    const open = settings.locator('#open-offline-cache');
    await open.click();
    const manager = page.locator('dialog:modal .offline-cache-manager');
    const rows = manager.locator('.offline-cache-list li');
    assert.ok(await manager.evaluate(el => el.closest('dialog') === window.settingsDialog), 'manager uses the same dialog');
    assert.equal(await page.locator('dialog:modal').count(), 1);
    await manager.locator('#offline-cache-back').click();
    await open.waitFor();
    await page.waitForFunction(() => document.activeElement.id === 'open-offline-cache');
    assert.ok(await open.evaluate(el => el.closest('dialog') === window.settingsDialog));
    await open.click();
    const next = manager.getByRole('button',{name:'Next page'});
    const prev = manager.getByRole('button',{name:'Previous page'});
    await rows.first().waitFor();
    assert.equal(await rows.count(),25);
    assert.ok(await prev.isDisabled());
    const visited = [];
    for (const pageSize of [25,25,2]) {
        if (visited.length) await next.click();
        assert.equal(await rows.count(),pageSize);
        visited.push(...await rows.evaluateAll(items => items.map(item => item.textContent.match(/Cache track \d+/)?.[0])));
    }
    assert.equal(new Set(visited).size,52, 'each saved track appears once across the three pages');
    assert.ok(await next.isDisabled());
    await prev.click();
    assert.equal(await rows.count(),25);
    const search = manager.locator('#offline-cache-search');
    await search.fill('cAcHe TrAcK 31');
    await rows.getByText('Cache track 31',{exact:true}).waitFor();
    assert.equal(await rows.count(),1,'search covers the entire cache');
    await search.fill('no match for this');
    await manager.locator('.offline-cache-empty').waitFor();
    assert.equal(await rows.count(),0);
    await search.fill('');
    const sort = manager.locator('#offline-cache-sort');
    const direction = manager.locator('#offline-cache-direction');
    async function sortBy(key, ascending, first) {
        await sort.selectOption(key);
        if (await direction.getAttribute('aria-pressed') !== String(ascending)) await direction.click();
        await rows.first().getByText(first,{exact:true}).waitFor();
    }
    await sortBy('size',false,'Cache track 52');
    await sortBy('downloaded',false,'Cache track 52');
    await sortBy('plays',false,'Cache track 07');
    await sortBy('last_played',false,'Cache track 17');
    await sortBy('quality',false,'Cache track 09');
    await sortBy('title',true,'Cache track 01');
    async function checkFit() {
        assert.ok(await manager.evaluate(el => {
            const dialog = el.closest('dialog'), bounds = dialog.getBoundingClientRect();
            return el.scrollWidth <= el.clientWidth+1 && dialog.scrollWidth <= dialog.clientWidth+1 &&
                bounds.left >= -1 && bounds.right <= innerWidth+1 && document.documentElement.scrollWidth <= innerWidth+1;
        }), 'cache manager fits the viewport at '+width+'px');
    }
    await checkFit();
    await page.screenshot({path:`target/offline-cache-manager-${width}.png`});
    const cleanup = manager.locator('.offline-cache-cleanup');
    await cleanup.locator('summary').click();
    const field = cleanup.locator('#offline-clean-by'), date = cleanup.locator('#offline-clean-date');
    async function preview(fieldName, day, amount, bytes) {
        await field.selectOption(fieldName);
        await date.fill(day);
        await cleanup.getByRole('button',{name:'Preview removal'}).click();
        const confirm = manager.locator('.offline-cache-confirm');
        await confirm.waitFor();
        assert.ok(await confirm.evaluate(el=>el.matches('dialog:modal')),'destructive preview is modal');
        await page.waitForFunction(() => document.activeElement === document.querySelector('.offline-cache-confirm button'));
        assert.match(await confirm.locator('.offline-cache-confirm-count').innerText(),new RegExp('\\b'+amount+'\\b'));
        const formatted = await page.evaluate(bytes => Alpine.store('offline').formatBytes(bytes),bytes);
        assert.ok((await confirm.locator('.offline-cache-confirm-bytes').innerText()).includes(formatted),
            'preview counts bytes actually freed, including shared files');
        await checkFit();
        return confirm;
    }
    const count = () => page.evaluate(() => Alpine.store('offline').items.length);
    const before = await page.evaluate(() => Alpine.store('offline').bytes);
    let confirm = await preview('downloaded','2023-01-10',3,5000);
    await page.screenshot({path:`target/offline-cache-cleanup-${width}.png`});
    await confirm.getByRole('button',{name:'Cancel'}).click();
    await confirm.waitFor({state:'hidden'});
    assert.equal(await count(),52);
    await page.waitForFunction(() => document.activeElement.textContent.trim() === 'Preview removal');
    confirm = await preview('downloaded','2023-01-10',3,5000);
    await page.keyboard.press('Escape');
    await confirm.waitFor({state:'hidden'});
    assert.equal(await count(),52);
    confirm = await preview('downloaded','2023-01-10',3,5000);
    await page.mouse.click(1,1);
    await confirm.waitFor({state:'hidden'});
    assert.equal(await count(),52);
    confirm = await preview('downloaded','2023-01-10',3,5000);
    await confirm.getByRole('button',{name:'Remove',exact:true}).click();
    await page.waitForFunction(() => Alpine.store('offline').items.length === 49);
    assert.equal(await page.evaluate(() => Alpine.store('offline').bytes),before - 5000);
    assert.ok(await page.evaluate(() => new OfflineCatalog('1').has(4)),'retains the shared edition on the cutoff date');
    confirm = await preview('last_played','2024-01-10',2,11000);
    await page.evaluate(() => new OfflineCatalog('1').touch(6));
    await confirm.getByRole('button',{name:'Remove',exact:true}).click();
    await page.waitForFunction(() => Alpine.store('offline').items.length === 48);
    const remaining = await page.evaluate(() => Alpine.store('offline').items.map(t=>t.id));
    assert.ok(!remaining.includes(5) && remaining.includes(6),'keeps newly played tracks and expires old never-played tracks');
    await search.fill('Cache track 52');
    await rows.getByRole('button',{name:'Remove from this device · Cache track 52'}).click();
    await manager.locator('.offline-cache-empty').waitFor();
    assert.equal(await count(),47);
    await cleanup.getByRole('button',{name:'Clear cache'}).click();
    confirm = manager.locator('.offline-cache-confirm');
    assert.match(await confirm.locator('.offline-cache-confirm-count').innerText(),/\b47\b/,
        'clearing the cache includes tracks beyond the current search and page');
    await confirm.getByRole('button',{name:'Remove',exact:true}).click();
    await page.waitForFunction(() => Alpine.store('offline').items.length === 0 && Alpine.store('offline').bytes === 0);
    await manager.locator('#offline-cache-back').click();
    await open.waitFor();
    await page.locator('.user-settings-head button:visible').last().click();
    assert.ok(await page.evaluate(() => Alpine.store('offline').manual),'editing the cache stays offline');
    const requests = networkLog.slice(baseline).filter(r=>!['/offline-worker.js','/offline-catalog.js'].includes(r.p));
    assert.deepEqual(requests,[], 'managing local storage never uses the network');
}

module.exports = {checkCacheManager};

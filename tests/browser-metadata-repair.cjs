// Run against a fresh disposable instance with tests/metadata_repair_fixture.sql.
// FURU_REPAIR_TEST_PG must name its disposable PostgreSQL container.
const {chromium}=require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assert=require('node:assert/strict');
const {readFileSync}=require('node:fs');
const {spawnSync}=require('node:child_process');
const base=process.env.FURU_TEST_URL || 'http://127.0.0.1:3029';
const container=process.env.FURU_REPAIR_TEST_PG || 'furumi-repair-test-pg';
function sql(statement) {
    const result=spawnSync('docker',['exec','-i',container,'psql','-U','postgres','-d','furumi_repair_test','-At','-v','ON_ERROR_STOP=1'],{input:statement,encoding:'utf8'});
    assert.equal(result.status,0,result.stderr);
    return result.stdout.trim();
}
const emptyPlan=()=>({artists:[],releases:[],tracks:[],questions:[],delete:{artists:[],releases:[],tracks:[]}});
(async()=>{
    const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_PATH || '/run/current-system/sw/bin/chromium',args:['--no-sandbox']});
    try {
        const context=await browser.newContext({viewport:{width:1440,height:1000}});
        if (process.env.FURU_TEST_ASSETS) {
            for (const [url,file] of [['alpinejs@3/dist/cdn.min.js','alpine.js'],['lucide@latest/dist/umd/lucide.min.js','lucide.js']]) {
                await context.route('https://cdn.jsdelivr.net/npm/'+url,route=>route.fulfill({path:process.env.FURU_TEST_ASSETS+'/'+file}));
            }
        }
        const api=context.request;
        assert.equal((await api.post(base+'/admin/v2/api/metadata-fix/apply',{data:{snapshot:{},plan:{},instruction:'test'}})).status()>=400,true);
        const page=await context.newPage(); const errors=[]; page.on('pageerror',error=>errors.push(error.message));
        await page.goto(base+'/admin/setup');
        if (page.url().includes('/admin/setup')) {
            await page.locator('[name=username]').fill('repair_admin');
            await page.locator('[name=password]').fill('repair-test-password');
            await page.locator('[name=confirm_password]').fill('repair-test-password');
            await page.locator('button[type=submit]').click();
            await page.waitForURL('**/admin/v2');
        } else {
            await page.goto(base+'/login');
            await page.locator('[name=username]').fill('repair_admin');
            await page.locator('[name=password]').fill('repair-test-password');
            await page.locator('button[type=submit]').click();
            await page.waitForURL(base+'/');
            await page.goto(base+'/admin/v2');
        }
        if (!process.env.FURU_REPAIR_SKIP_SEED) sql(readFileSync(__dirname+'/metadata_repair_fixture.sql','utf8'));
        const post=async(path,data,status=200)=>{
            const response=await api.post(base+'/admin/v2/api/metadata-fix/'+path,{data,timeout:240000});
            assert.equal(response.status(),status,await response.text());
            return response.json();
        };
        const expanded=await post('expand',{selection:{artists:[2719],releases:[],tracks:[]}});
        const before=expanded.snapshot;
        assert.equal(before.releases.length,4); assert.equal(before.tracks.length,10);
        // Reproduce the reported request using ONLY the four release checkboxes.
        if (process.env.FURU_REPAIR_TEST_MODEL) {
            const selection={artists:[],releases:before.releases.map(r=>r.id),tracks:[]};
            const instruction=`релизы #ИЗИРОК на самом деле один релиз типа компиляция 2021 года. тебе нужно сделать план по объединению в один релиз. вот полный треклист по порядку 01. Старик и братья
02. Потрошитель
03. Последняя песня
04. Песня для твоей подруги
05. Просто я сегодня... (feat. Перегаз)
06. Отзвуки тьмы
07. Битва за Москву
08. Настроение лето
09. Пробил час
10. Цусима`;
            const proposed=await post('plan',{selection,instruction});
            assert.equal(proposed.snapshot.artists.length,0);
            assert.equal(proposed.snapshot.tracks.length,10);
            assert.deepEqual(proposed.plan.artists,[]);
            assert.deepEqual(proposed.plan.delete.artists,[]);
            assert.deepEqual(proposed.plan.delete.tracks,[]);
            const destination=proposed.plan.releases.find(r=>r.release_type==='compilation' && r.year===2021);
            assert.ok(destination,JSON.stringify(proposed.plan));
            const target=destination.id ? 'existing:'+destination.id : 'new:'+destination.key;
            for (const [index,track] of before.tracks.entries()) {
                const edit=proposed.plan.tracks.find(t=>t.id===track.id);
                assert.equal(edit?.target_release || 'existing:'+track.release_id,target);
                assert.equal(edit?.track_number ?? track.track_number,index+1);
                assert.deepEqual(edit?.artist_ids || [2719],[2719]);
            }
            const result=await post('apply',{snapshot:proposed.snapshot,plan:proposed.plan,instruction});
            assert.equal(sql('SELECT count(*) FROM furumusic__track'),'10');
            assert.equal(sql('SELECT count(*) FROM furumusic__release'),'1');
            assert.equal(sql('SELECT name FROM furumusic__artist'),'Radio Tapok');
            await post('undo',{history_id:result.history_id});
            assert.deepEqual((await post('expand',{selection:expanded.selection})).snapshot,before);
            const excluded=await post('plan',{selection,excluded_track_ids:[33683],instruction:'Ничего не меняй. Верни пустой план.'});
            assert.equal(excluded.snapshot.tracks.length,9);
            assert.equal(excluded.snapshot.tracks.some(t=>t.id===33683),false);
            assert.deepEqual(excluded.plan.tracks,[]);
            const recovered=await post('plan',{selection,instruction:'__TEST_INVALID_ARTIST__ Объедини четыре релиза в компиляцию 2021 года.'});
            assert.deepEqual(recovered.plan.artists,[]);
            assert.equal(recovered.plan.releases[0].year,2021);
            console.log('Release-only scope, exact ordered tracklist, model correction and exclusions passed.');
        }
        const artistIds=track=>before.track_artists.filter(link=>link.owner_id===track.id).map(link=>link.artist_id);
        const merge={...emptyPlan(),releases:[{id:null,key:'collection',title:'#ИЗИРОК Сборник',release_type:'compilation',year:2021,artist_ids:[2719]}],tracks:before.tracks.map(track=>({id:track.id,title:track.title,target_release:'new:collection',track_number:track.track_number,disc_number:track.disc_number,year:track.year,artist_ids:artistIds(track)})),delete:{artists:[],releases:before.releases.map(release=>release.id),tracks:[]}};
        await page.getByRole('button',{name:'Library Tools',exact:true}).click();
        const tools=page.locator('.content').filter({has:page.getByText('Metadata assistant',{exact:true})});
        await tools.locator('select').first().selectOption('artists');
        await tools.getByPlaceholder('Search library…').fill('Radio Tapok');
        await tools.locator('.repair-search-results button').first().click();
        await tools.getByRole('button',{name:'Show related',exact:true}).click();
        await page.waitForFunction(()=>Alpine.$data(document.querySelector('.admin-shell')).repairRowsCount()===15);
        await tools.getByRole('button',{name:'Deselect all',exact:true}).click();
        assert.equal(await tools.locator('.repair-entity input:checked').count(),0);
        assert.equal(await tools.locator('.repair-entity input[type=checkbox]').count(),15);
        for (const release of before.releases) {
            await tools.getByRole('checkbox',{name:new RegExp('^#ИЗИРОК #' + release.id + ' ')}).check();
        }
        assert.equal(await tools.locator('.repair-entity input:checked').count(),14);
        assert.equal(await tools.getByRole('checkbox',{name:'Radio Tapok #2719',exact:true}).isChecked(),false);
        let calls=0;
        await page.route('**/metadata-fix/plan',async route=>{
            const body=route.request().postDataJSON(); calls++;
            assert.deepEqual(body.selection,{...expanded.selection,artists:[]});
            if (calls===1) await route.fulfill({json:{snapshot:before,plan:merge,message:'Объединю все 10 треков в сборник 2021 года.'}});
            else {
                assert.equal(body.current_plan.releases[0].year,2021);
                assert.equal(body.conversation.length,2);
                await route.fulfill({json:{snapshot:before,plan:{...merge,releases:[{...merge.releases[0],year:2022}]},message:'Год сборника изменён на 2022.'}});
            }
        });
        const composer=tools.getByRole('textbox',{name:'Message to metadata assistant'});
        await composer.fill('Объедини #ИЗИРОК в #ИЗИРОК Сборник, год 2021');
        await tools.getByRole('button',{name:'Send',exact:true}).click();
        await tools.getByText('Объединю все 10 треков в сборник 2021 года.',{exact:true}).waitFor();
        const plan=tools.locator('details.repair-plan');
        assert.equal(await plan.getAttribute('open'),null);
        await plan.locator(':scope > summary').click();
        await plan.getByText('Edit fields manually',{exact:true}).click();
        assert.equal(await plan.locator('.repair-diff').nth(1).locator('input[type=number]').inputValue(),'2021');
        await composer.fill('Сделай год 2022');
        await tools.getByRole('button',{name:'Send',exact:true}).click();
        await tools.getByText('Год сборника изменён на 2022.',{exact:true}).waitFor();
        assert.equal(await tools.locator('details.repair-plan').count(),1);
        assert.equal(await plan.locator('.repair-diff').nth(1).locator('input[type=number]').inputValue(),'2022');
        await plan.getByText('Edit fields manually',{exact:true}).click();
        await plan.locator(':scope > summary').scrollIntoViewIfNeeded();
        await page.screenshot({path:'/tmp/furumi-repair-desktop.png'});
        await page.setViewportSize({width:390,height:844});
        assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
        await page.screenshot({path:'/tmp/furumi-repair-mobile.png'});
        await page.setViewportSize({width:1440,height:1000});
        // Apply the 2021 plan against the original snapshot, then undo it.
        const applied=await post('apply',{snapshot:before,plan:merge,instruction:'Merge #ИЗИРОК'});
        assert.equal(sql("SELECT title || '|' || year || '|' || total_tracks FROM furumusic__release"),'#ИЗИРОК Сборник|2021|10');
        assert.equal(sql('SELECT count(*) FROM furumusic__track'),'10');
        assert.equal(sql('SELECT count(*) FROM furumusic__media_file'),'10');
        assert.equal(sql('SELECT count(*) FROM furumusic__playlist_track'),'2');
        await post('undo',{history_id:applied.history_id});
        assert.deepEqual((await post('expand',{selection:expanded.selection})).snapshot,before);
        const deletion={...emptyPlan(),delete:{artists:[],releases:[],tracks:[33683,33684]}};
        const removed=await post('apply',{snapshot:before,plan:deletion,instruction:'Delete two tracks'});
        assert.equal(sql('SELECT count(*) FROM furumusic__track'),'8');
        assert.equal(sql('SELECT count(*) FROM furumusic__playlist_track'),'0');
        assert.equal(sql('SELECT count(*) FROM furumusic__track_ref WHERE local_track_id IS NULL'),'2');
        assert.equal(sql('SELECT queue_json FROM furumusic__playback_state'),'[33685]');
        await post('undo',{history_id:removed.history_id});
        assert.equal(sql('SELECT count(*) FROM furumusic__playlist_track'),'2');
        assert.equal(sql('SELECT local_track_id FROM furumusic__track_ref ORDER BY content_id'),'33683\n33684');
        assert.equal(sql('SELECT current_track_id || \'|\' || position_ms || \'|\' || queue_json FROM furumusic__playback_state'),'33683|1234|[33683,33684,33685]');
        assert.deepEqual((await post('expand',{selection:expanded.selection})).snapshot,before);
        const artistRemoval=await post('apply',{snapshot:before,plan:{...emptyPlan(),delete:{artists:[2719],releases:[],tracks:[]}},instruction:'Delete artist'});
        assert.equal(sql('SELECT count(*) FROM furumusic__artist'),'0');
        assert.equal(sql('SELECT count(*) FROM furumusic__track'),'10');
        assert.equal(sql('SELECT count(*) FROM furumusic__track_artist'),'0');
        await post('undo',{history_id:artistRemoval.history_id});
        assert.deepEqual((await post('expand',{selection:expanded.selection})).snapshot,before);
        const invalid={...emptyPlan(),delete:{artists:[],releases:[4256],tracks:[]}};
        await post('apply',{snapshot:before,plan:invalid,instruction:'Invalid deletion'},400);
        // A changed detached reference must block undo atomically.
        const conflict=await post('apply',{snapshot:before,plan:deletion,instruction:'Delete before playback changes'});
        sql("UPDATE furumusic__playback_state SET position_ms=9876");
        await post('undo',{history_id:conflict.history_id},409);
        assert.equal(sql('SELECT count(*) FROM furumusic__track'),'8');
        sql('UPDATE furumusic__playback_state SET position_ms=0');
        await post('undo',{history_id:conflict.history_id});
        assert.equal(sql('SELECT count(*) FROM furumusic__track'),'10');
        assert.deepEqual(errors,[]);
        console.log('Metadata Repair browser, merge, deletion, undo and conflict checks passed.');
    } finally { await browser.close(); }
})().catch(error=>{console.error(error);process.exit(1);});

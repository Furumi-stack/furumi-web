// Run only against a fresh disposable instance, with the import agent disabled.
const {chromium}=require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const assert=require('node:assert/strict');
const {spawnSync}=require('node:child_process');
const base=process.env.FURU_TEST_URL || 'http://127.0.0.1:3019';
const container=process.env.FURU_TEST_PG_CONTAINER || 'furumi-import-playlist-test-pg';
function sql(statement) {
    const result=spawnSync('docker',['exec','-i',container,'psql','-U','postgres','-d','furumi_import_test','-At','-v','ON_ERROR_STOP=1'],{input:statement,encoding:'utf8'});
    assert.equal(result.status,0,result.stderr);return result.stdout.trim();
}
(async()=>{
    const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_PATH || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'});
    try {
        const context=await browser.newContext({viewport:{width:1440,height:1000}});
        const page=await context.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
        await page.goto(base+'/admin/setup');
        await page.locator('[name=username]').fill('importer');
        await page.locator('[name=password]').fill('import-test-password');
        await page.locator('[name=confirm_password]').fill('import-test-password');
        await page.locator('form button[type=submit]').click();await page.waitForURL('**/admin/v2');
        await page.goto(base+'/');
        await page.evaluate(()=>{Alpine.store('torrents').open();Alpine.store('torrents').showSourceTab('files');});
        await page.locator('#direct-audio-file-input').setInputFiles([
            {name:'theme.wav',mimeType:'audio/wav',buffer:Buffer.from('test-audio-one')},
            {name:'credits.wav',mimeType:'audio/wav',buffer:Buffer.from('test-audio-two')},
        ]);
        await page.locator('#local-target-playlist').selectOption('__new__');
        await page.getByPlaceholder('New playlist name',{exact:true}).fill('Game Soundtrack');
        await page.screenshot({path:'/tmp/furumi-import-desktop.png'});
        await page.setViewportSize({width:390,height:844});
        await page.screenshot({path:'/tmp/furumi-import-mobile.png'});
        await page.getByRole('button',{name:'Upload selected files',exact:true}).click();
        await page.waitForFunction(()=>!Alpine.store('torrents').localFilesUploading && Alpine.store('torrents').localFiles.length===0);
        const history=await (await context.request.get(base+'/api/player/uploads/local/history')).json();
        assert.equal(history.length,2,JSON.stringify(history));
        const target=history[0].target_playlist_id;assert.ok(target>0);assert.equal(history[1].target_playlist_id,target);
        assert.equal(sql('SELECT count(*) FROM furumusic__import_playlist_target;'),'2');
        // Simulate the importer resolving both uploaded files to library tracks.
        sql(`INSERT INTO furumusic__media_file (file_type,file_path,original_filename,mime_type,file_size_bytes,sha256_hash,created_at)
             SELECT 'audio',input_path,input_path,'audio/wav',14,source_sha256,'2026-11-12T00:00:00Z' FROM furumusic__import_playlist_target;
             INSERT INTO furumusic__track (title,title_sort,release_id,duration_seconds,audio_file_id,is_hidden,created_at,updated_at)
             SELECT original_filename,original_filename,0,120,id,false,'2026-11-12T00:00:00Z','2026-11-12T00:00:00Z' FROM furumusic__media_file;`);
        assert.equal((await context.request.get(base+'/api/player/uploads/local/history')).status(),200);
        assert.equal(sql(`SELECT count(*) FROM furumusic__playlist_track WHERE playlist_id=${target};`),'2');
        await context.request.get(base+'/api/player/uploads/local/history');
        assert.equal(sql(`SELECT count(*) FROM furumusic__playlist_track WHERE playlist_id=${target};`),'2');
        const invalid=await context.request.post(base+'/api/player/uploads/local',{headers:{'X-Furumusic-Target-Playlist-Id':'99999','X-Furumusic-Filename':'bad.wav'},data:Buffer.from('bad')});
        assert.equal(invalid.status(),400);
        // Render the torrent picker with a local preview fixture; no external torrent network.
        await page.setViewportSize({width:1440,height:1000});
        await page.evaluate(()=>{
            const s=Alpine.store('torrents');s.showSourceTab('torrents');s._stopRefresh();s._stopPoll();
            s._applySession({job:{id:'preview-test',name:'Movie OST',status:'preview'},preview:{id:'preview-test',name:'Movie OST',total_size:20,files:[{index:0,name:'theme.flac',components:['theme.flac'],length:20,selected:true}]},selected_files:[0]});
        });
        await page.locator('#torrent-target-playlist').selectOption(String(target));
        assert.equal(await page.evaluate(()=>Alpine.store('torrents').torrentPlaylistChoice),String(target));
        await page.screenshot({path:'/tmp/furumi-torrent-playlist.png'});
        await page.evaluate(()=>{Alpine.store('torrents').currentJob.status='paused';});
        assert.equal(await page.locator('#torrent-target-playlist').isDisabled(),true);
        assert.deepEqual(errors,[]);
        console.log('PASS: file batch → new playlist, persistent destinations, imported tracks, idempotency, invalid destination, torrent picker, desktop/mobile');
    } finally {await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1});

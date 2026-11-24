// Render the player fixture with FURU_PODCAST_RENDER_FIXTURE=1 before running.
const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {chromium} = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = readFileSync('target/podcast-player-fixture.html', 'utf8');
const alpine = readFileSync('target/podcast-alpine.min.js', 'utf8');
const image = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=', 'base64');
const covers = Array.from({length:5}, (_, i) => ({
    cover_id:i+5, title:i===4 ? 'Alternate track artwork' : 'Album '+i, cover_url:'/api/player/cover/'+(i+5)+'/small',
}));
const automatic = covers.slice(0,4).map(cover => cover.cover_url);

async function checkHeader(browser, width) {
    const mobile = width < 600;
    const page = await browser.newPage({viewport:{width,height:824}, hasTouch:mobile, isMobile:mobile});
    page.setDefaultTimeout(5000);
    const errors = [], writes = [];
    page.on('pageerror', err => errors.push(err.message));
    const tracks = covers.map(({cover_url}, i) => ({
        id:i+2, playlist_track_id:i+7, title:'Track '+i, release_id:30+i%4, release_title:'Album '+(i%4), cover_url,
        artists:[{id:1,name:'One Artist'}], featured_artists:i % 2 === 0 ? [{id:2,name:'Гость'}] : [], duration_seconds:210,
    }));
    const owned = {id:7,title:'My mix',kind:'user',is_own:true,owner_name:'Listener',track_count:tracks.length,
        tracks, cover_mode:'auto',cover_ids:[],cover_urls:automatic,auto_cover_urls:automatic,cover_url:automatic[0],cover_options:covers};
    const shared = {...owned,id:8,title:'Shared mix',is_own:false,is_public:true,owner_name:'Someone'};
    const likes = {...owned,id:-1,kind:'likes',title:'Likes'};
    let failNext = false, pins = [], delayedDelete, finishFederation;
    const firstFederation = new Promise(resolve => {finishFederation = resolve;});
    const trackWrites = [];
    await page.route('**/*', async route => {
        const request = route.request(), {hostname,pathname} = new URL(request.url());
        if (hostname === 'playlist.test' && pathname === '/') return route.fulfill({contentType:'text/html',body:html});
        if (hostname === 'cdn.jsdelivr.net' && pathname.includes('alpine')) return route.fulfill({contentType:'application/javascript',body:alpine});
        if (pathname.startsWith('/api/player/cover/') || pathname === '/api/player/playlists/7/cover')
            return route.fulfill({contentType:'image/png',body:image});
        if (pathname === '/api/player/playlists/7/federation-tracks') {
            await firstFederation;
            return route.fulfill({json:[]});
        }
        if (pathname === '/api/player/playlists/8/federation-tracks') return route.fulfill({json:[{position:99,
            federation:{title:'Peer-only track',owner:'peer',item_id:'peer-track',content_id:'b3:'+'a'.repeat(64),artist_names:['Other artist']}}]});
        if (pathname === '/api/player/pins' && request.method() === 'POST') {
            const payload = request.postDataJSON();
            pins = payload.pinned ? [{...payload,kind:'playlist'}] : [];
        }
        if (pathname === '/api/player/playlists/7/tracks' && ['DELETE','PUT'].includes(request.method())) {
            const data = request.postDataJSON();
            trackWrites.push(data);
            if (request.method() === 'DELETE' && delayedDelete) await delayedDelete;
            owned.tracks = request.method() === 'DELETE'
                ? owned.tracks.filter(track => track.playlist_track_id !== data.playlist_track_id)
                : data.playlist_track_ids.map(id => owned.tracks.find(track => track.playlist_track_id === id));
            const options = owned.tracks.map(track => covers.find(c => c.cover_url === track.cover_url));
            const urls = options.slice(0,4).map(c => c.cover_url);
            const artwork = {cover_mode:'auto',cover_ids:[],cover_urls:urls,auto_cover_urls:urls,cover_options:options,cover_url:urls[0]};
            Object.assign(owned,artwork);
            return route.fulfill({json:{ok:true,artwork}});
        }
        if (pathname === '/api/player/playlists/7' && request.method() === 'PUT') {
            const data = request.postDataJSON();
            writes.push(data);
            if (failNext) {
                failNext = false;
                return route.fulfill({status:503,json:{error:'Could not save'}});
            }
            if (data.title != null) owned.title = data.title;
            if (data.cover_mode) {
                owned.cover_mode = data.cover_mode;
                owned.cover_ids = data.cover_ids || [];
                if (data.cover_mode === 'auto') owned.cover_urls = automatic;
                if (data.cover_mode === 'releases')
                    owned.cover_urls = owned.cover_ids.map(id => covers.find(r => r.cover_id === id).cover_url);
                if (data.cover_mode === 'upload') owned.cover_urls = ['/api/player/playlists/7/cover?v=test'];
                owned.cover_url = owned.cover_urls[0] || null;
            }
            return route.fulfill({json:{ok:true,title:owned.title,cover_mode:owned.cover_mode,cover_urls:owned.cover_urls,
                cover_ids:owned.cover_ids,cover_url:owned.cover_url}});
        }
        const routes = {
            '/api/player/playlists':[owned,shared,likes], '/api/player/playlists/7':owned,
            '/api/player/playlists/8':shared,'/api/player/playlists/-1':likes,
            '/api/player/me':{id:1,name:'Listener',role:'user'}, '/api/player/pins':pins,
            '/api/player/artists':{items:[],total:0,has_more:false},
        };
        return route.fulfill({json:routes[pathname] || []});
    });

    const heading = page.locator('.playlist-detail-heading');
    const cover = heading.locator('.playlist-heading-cover');
    const pin = heading.locator('.collection-pin');
    const edit = heading.locator('.playlist-edit-toggle');
    const input = heading.locator('.playlist-title-input');
    const editor = page.locator('.playlist-cover-editor:visible');
    const activate = el => mobile ? el.tap() : el.click();
    const chooseMode = mode => activate(editor.locator('[data-mode="'+mode+'"]'));
    const verifyArt = urls => page.waitForFunction(expected => {
        const shown = Array.from(document.querySelectorAll('.playlist-heading-cover img')).filter(img => img.offsetWidth);
        return shown.length === expected.length && shown.every((img,i) => img.getAttribute('src') === expected[i] && img.naturalWidth);
    }, urls);
    async function showCoverEditor() {
        await activate(heading.locator('.playlist-cover-toggle'));
        await editor.waitFor();
    }
    await page.goto('http://playlist.test/#playlist/7');
    await heading.getByRole('heading',{name:'My mix'}).waitFor();
    await verifyArt(automatic);
    finishFederation(); // Local details and artwork were already visible.
    const headerBox = await heading.boundingBox(), pinBox = await pin.boundingBox(), artBox = await cover.boundingBox();
    assert.ok(pinBox.x > headerBox.x+headerBox.width*.65 && pinBox.x > artBox.x+artBox.width, 'pin is at the right of the header');
    await activate(pin);
    await page.waitForFunction(() => document.querySelector('.playlist-detail-heading .collection-pin').getAttribute('aria-pressed') === 'true');

    await activate(edit);
    await input.fill('Draft that will be discarded');
    const decoration = await input.evaluate(el => {
        const s = getComputedStyle(el);
        const color = parseFloat(s.borderBottomWidth) > 0 ? s.borderBottomColor : s.boxShadow;
        const rgb = /rgb\((\d+), (\d+), (\d+)\)/.exec(color);
        return {italic:s.fontStyle === 'italic',underline:rgb && +rgb[2] > +rgb[1] && +rgb[2] > +rgb[3]};
    });
    assert.ok(decoration.italic && decoration.underline,'editable title is italic with a green underline');
    const headerLayout = () => heading.evaluate(el => {
        const row = document.querySelector('.playlist-track-row');
        return [el.getBoundingClientRect().height, row.getBoundingClientRect().top, row.closest('.main-content').scrollHeight];
    });
    const initialLayout = await headerLayout();
    await showCoverEditor();
    assert.deepEqual(await headerLayout(), initialLayout, 'opening the artwork picker leaves the track list in place');
    await chooseMode('releases');
    assert.deepEqual(await headerLayout(), initialLayout, 'showing thumbnails keeps the header and page height stable');
    const popup = await editor.boundingBox(), anchor = await cover.boundingBox();
    assert.ok(popup.x >= 0 && popup.x + popup.width <= width + 1);
    assert.ok(Math.abs(popup.y - anchor.y - anchor.height) < 12 ||
        Math.abs(anchor.y - popup.y - popup.height) < 12, 'the picker floats next to the artwork');
    await activate(input);
    await editor.waitFor({state:'hidden'});
    assert.equal(await input.inputValue(), 'Draft that will be discarded', 'outside click preserves the header draft');
    await showCoverEditor();
    await page.keyboard.press('Escape');
    await editor.waitFor({state:'hidden'});
    assert.ok(await input.isVisible(),'closing the cover picker keeps the title draft');
    assert.ok(await heading.locator('.playlist-cover-toggle').evaluate(button => document.activeElement === button),
        'closing the cover picker restores focus to its trigger');
    await activate(heading.locator('.playlist-edit-cancel'));
    await heading.getByRole('heading',{name:'My mix'}).waitFor();
    await verifyArt(automatic);
    assert.equal(writes.length,0,'cancel does not save a draft');

    await activate(edit);
    await input.fill('Late night set');
    await showCoverEditor();
    await chooseMode('releases');
    const choices = editor.locator('.playlist-cover-choice');
    assert.equal(await choices.count(),covers.length);
    for (const choice of await choices.all()) {
        if (await choice.getAttribute('aria-pressed') === 'true') await activate(choice);
    }
    for (const [index,i] of [4,2,0,1].entries()) {
        await activate(choices.nth(i));
        await verifyArt([4,2,0,1].slice(0,index+1).map(j => covers[j].cover_url));
        const coverage = await cover.evaluate(el => {
            const tiles = el.querySelector('.playlist-art-tiles').getBoundingClientRect();
            return Array.from(el.querySelectorAll('img')).reduce((sum,img) => {
                const r = img.getBoundingClientRect();
                return sum+r.width*r.height;
            },0)/(tiles.width*tiles.height);
        });
        assert.ok(coverage > .95 && coverage <= 1.01, 'one to four covers fill the collage');
    }
    assert.equal(await editor.locator('.playlist-cover-choice[aria-pressed="true"]').count(),4);
    const extra = choices.nth(3);
    if (!await extra.isDisabled()) await activate(extra);
    assert.equal(await editor.locator('.playlist-cover-choice[aria-pressed="true"]').count(),4,'collages have four slots');
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth),false);
    failNext = true;
    await activate(edit);
    await page.locator('.playlist-edit-error:visible').waitFor();
    assert.equal(await input.inputValue(),'Late night set');
    await activate(edit);
    await heading.getByRole('heading',{name:'Late night set'}).waitFor();
    assert.deepEqual(writes[1],writes[0],'retry preserves the draft');
    assert.deepEqual(writes[1].cover_ids,[9,7,5,6],'cover order follows the user selection');
    assert.equal(writes[1].cover_mode,'releases');
    await page.reload();
    await verifyArt(owned.cover_urls);

    await activate(edit);
    await showCoverEditor();
    await chooseMode('upload');
    await editor.locator('input[type="file"]').setInputFiles({name:'cover.txt',mimeType:'text/plain',buffer:Buffer.from('not an image')});
    await page.locator('.playlist-edit-error:visible').waitFor();
    assert.ok(await edit.isDisabled(),'invalid file cannot be saved as a cover');
    await editor.locator('input[type="file"]').setInputFiles({name:'cover.png',mimeType:'image/png',buffer:image});
    await activate(edit);
    await input.waitFor({state:'hidden'});
    assert.equal(writes.at(-1).cover_mode,'upload');
    assert.deepEqual(Buffer.from(writes.at(-1).cover_image,'base64'),image);
    await page.reload();
    await verifyArt(owned.cover_urls);
    await activate(edit);
    await showCoverEditor();
    await chooseMode('auto');
    await activate(edit);
    await input.waitFor({state:'hidden'});
    assert.equal(writes.at(-1).cover_mode,'auto');
    await verifyArt(automatic);

    await activate(edit);
    const draftTitle = 'Unsaved while changing tracks';
    await input.fill(draftTitle);
    await showCoverEditor();
    await chooseMode('releases');
    await activate(choices.nth(4));
    await activate(choices.nth(1));
    await page.keyboard.press('Escape');
    await editor.waitFor({state:'hidden'});
    const rows = page.locator('.playlist-track-row');
    const trackFilter = page.locator('#playlist-track-search');
    await trackFilter.fill('ГОСТЬ');
    await page.waitForFunction(() => document.querySelectorAll('.playlist-track-row').length === 3);
    assert.deepEqual(await rows.evaluateAll(elements => elements.map(row => Number(row.dataset.playlistIndex))), [0,2,4]);
    let finishDelete;
    delayedDelete = new Promise(resolve => {finishDelete = resolve;});
    const deleting = page.waitForRequest(req => req.method() === 'DELETE');
    await activate(rows.nth(2).locator('.playlist-track-remove'));
    await deleting;
    assert.equal(await edit.isDisabled(),true,'header save waits for the pending track update');
    await page.evaluate(() => Alpine.store('playlists').moveCurrentTrack(0,1));
    assert.deepEqual(trackWrites,[{playlist_track_id:11}],'concurrent changes cannot reorder a stale track list');
    finishDelete();
    delayedDelete = null;
    await verifyArt([covers[1].cover_url]);
    await showCoverEditor();
    assert.equal(await choices.count(),4,'deleted track artwork is no longer offered');
    assert.equal(await editor.locator('.playlist-cover-choice[aria-pressed="true"]').count(),1);
    assert.equal(await input.inputValue(),draftTitle);
    await chooseMode('auto');
    await activate(heading.locator('.playlist-cover-toggle'));
    assert.equal(await trackFilter.inputValue(), 'ГОСТЬ', 'refreshing the same playlist preserves its filter');
    await page.waitForFunction(() => document.querySelectorAll('.playlist-track-row').length === 2);
    if (mobile) {
        const handle = rows.nth(1).locator('.playlist-drag-handle');
        await handle.scrollIntoViewIfNeeded();
        const from = await handle.boundingBox(), to = await rows.first().boundingBox();
        const x = Math.round(from.x+from.width/2);
        const session = await page.context().newCDPSession(page);
        await session.send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[{x,y:from.y+from.height/2}]});
        await session.send('Input.dispatchTouchEvent',{type:'touchMove',touchPoints:[{x,y:to.y+to.height/2}]});
        await session.send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});
        await session.detach();
    } else {
        await rows.nth(1).dragTo(rows.first());
    }
    const reordered = [2,0,1,3].map(i => covers[i].cover_url);
    await verifyArt(reordered);
    assert.deepEqual(trackWrites[1],{playlist_track_ids:[9,7,8,10]});
    assert.equal(await input.inputValue(),draftTitle,'reordering keeps the header draft');
    await page.reload();
    await heading.getByRole('heading',{name:'Late night set'}).waitFor();
    await verifyArt(reordered);

    await activate(edit);
    delayedDelete = new Promise(resolve => {finishDelete = resolve;});
    const leaving = page.waitForRequest(req => req.method() === 'DELETE');
    await activate(rows.first().locator('.playlist-track-remove'));
    await leaving;
    for(const playlist of [shared,likes]) {
        await page.goto('http://playlist.test/#playlist/'+playlist.id);
        await page.waitForFunction(id => Alpine.store('library').currentPlaylist?.id === id, playlist.id);
        if (playlist === shared) {
            await rows.getByText('Peer-only track',{exact:true}).waitFor();
            finishDelete();
            await page.waitForFunction(() => !Alpine.store('playlists').changingTracks);
        }
        assert.equal(await edit.isVisible(),false,'only owners can edit ordinary playlists');
        assert.equal(await input.isVisible(),false);
        await verifyArt(automatic);
    }
    delayedDelete = null;
    await page.goto('http://playlist.test/#playlist/7');
    await activate(edit);
    delayedDelete = new Promise(resolve => {finishDelete = resolve;});
    const deletingAgain = page.waitForRequest(req => req.method() === 'DELETE');
    await activate(rows.first().locator('.playlist-track-remove'));
    await deletingAgain;
    await page.goto('http://playlist.test/#playlist/8');
    await heading.getByRole('heading',{name:'Shared mix'}).waitFor();
    await page.goto('http://playlist.test/#playlist/7');
    await page.waitForFunction(() => Alpine.store('library')._activeHash === '#playlist/7');
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    finishDelete();
    // Re-entering the same playlist waits for its old write before reading the new detail.
    await verifyArt([covers[1].cover_url,covers[3].cover_url]);
    assert.deepEqual(await rows.locator('.track-title').allTextContents(),['Track 1','Track 3']);
    assert.deepEqual(errors,[]);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({headless:true,...(process.env.CHROME_PATH ? {executablePath:process.env.CHROME_PATH} : {})});
    try {
        for(const width of [1440,320]) await checkHeader(browser,width);
        console.log('Playlist artwork, title edits, cancellation, retry and ownership passed on desktop and mobile.');
    } finally { await browser.close(); }
})().catch(error => { console.error(error); process.exitCode=1; });

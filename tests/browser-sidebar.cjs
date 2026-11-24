// Render the player fixture with FURU_PODCAST_RENDER_FIXTURE=1 before running.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');

const html = readFileSync('target/podcast-player-fixture.html', 'utf8');
const alpine = readFileSync('target/podcast-alpine.min.js', 'utf8');
const likes = { id: -1, kind: 'likes', title: 'Likes', track_count: 14, is_own: true };
const owned = { id: 7, kind: 'user', title: 'Initial D', track_count: 3, is_own: true };
const pin = { id: 9, kind: 'release', title: 'Ocean Rain', href: '#release/9' };

async function checkCollection(browser) {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    const saved = { id: 8, kind: 'user', title: 'Shared mix', is_saved: true, is_public: true, track_count: 4 };
    const publicMix = { id: 9, kind: 'user', title: 'Public mix', is_public: true, track_count: 6 };
    const artist = {id: 42, name: 'Echo', track_count: 1};
    let pins = [];
    await page.route('**/*', route => {
        const {hostname, pathname} = new URL(route.request().url());
        if (hostname === 'sidebar.test' && pathname === '/') return route.fulfill({contentType: 'text/html', body: html});
        if (hostname === 'cdn.jsdelivr.net' && pathname.includes('alpine')) return route.fulfill({contentType: 'application/javascript', body: alpine});
        if (pathname === '/api/player/pins' && route.request().method() === 'POST') {
            const item = route.request().postDataJSON();
            pins = item.pinned ? [{...item, kind: 'playlist'}] : [];
        }
        const responses = {
            '/api/player/playlists': [likes, publicMix, {...owned, is_public:true}, saved], '/api/player/pins': pins,
            '/api/player/me': {id: 1, name: 'Listener'}, '/api/player/follows': {artist_ids: [artist.id], artists: [artist]},
            '/api/player/artists': {items: [], total: 0, has_more: false},
        };
        return route.fulfill({json: responses[pathname] || []});
    });
    await page.goto('http://sidebar.test/#collection');
    await page.waitForURL('**/#artists?tab=playlists');
    const entries = page.locator('.collection-list');
    await entries.getByText(owned.title).waitFor();
    await entries.getByText(saved.title).waitFor();
    assert.deepEqual(await page.locator('.collection-tabs a').allTextContents(), ['All music', 'Playlists', 'My uploads']);
    assert.equal(await page.locator('.collection-tabs [aria-current="page"]').innerText(), 'Playlists');
    assert.deepEqual(await entries.locator('.collection-link').evaluateAll(links => links.map(a => a.hash)),
        ['#playlist/-1', '#playlist/7', '#playlist/9', '#playlist/8']);
    assert.equal(await entries.locator('.collection-options').count(), 1);
    await page.getByRole('button', {name:'New Playlist'}).click();
    assert.equal(await page.evaluate(() => Alpine.store('playlists').modal?.mode), 'create');
    await page.getByRole('button', {name:'Cancel', exact:true}).click();
    const ownedRow = entries.locator('.collection-row').filter({hasText: owned.title});
    await ownedRow.locator('.collection-more').click();
    await ownedRow.getByRole('button', {name: 'Rename'}).waitFor();
    await page.keyboard.press('Escape');
    await ownedRow.locator('.collection-popup').waitFor({state: 'hidden'});
    const pinButton = entries.locator('.collection-row').filter({hasText: saved.title}).locator('.collection-pin');
    const sidebar = page.locator('.sidebar-left .music-shortcuts');
    await pinButton.click();
    await sidebar.getByText(saved.title).waitFor();
    assert.equal(await pinButton.getAttribute('aria-pressed'), 'true');
    await page.reload();
    await sidebar.getByText(saved.title).waitFor();
    await pinButton.click();
    await sidebar.getByText(saved.title).waitFor({state: 'hidden'});
    await page.locator('.sidebar-account-trigger').click();
    await page.locator('#sidebar-account-actions').getByRole('link', {name:'Followed'}).click();
    await page.waitForURL('**/#followed');
    await entries.getByText(artist.name).waitFor();
    assert.equal(await entries.locator('a').getAttribute('href'), '#artist/42');
    assert.equal(await page.locator('.collection-tabs:visible').count(), 0);
    await page.goto('http://sidebar.test/#collection?tab=artists');
    await page.waitForURL('**/#followed');
    await entries.getByText(artist.name).waitFor();
    await page.locator('.sidebar-left a[href="#artists"]').click();
    await Promise.all([
        page.waitForRequest(req => new URL(req.url()).pathname === '/api/player/artists' && new URL(req.url()).searchParams.has('mine')),
        page.getByRole('link', {name:'My uploads', exact:true}).click(),
    ]);
    await page.waitForURL('**/#uploads');
    assert.equal(await page.locator('.collection-tabs [aria-current="page"]').innerText(), 'My uploads');
    await Promise.all([
        page.waitForRequest(req => new URL(req.url()).pathname === '/api/player/artists' && !new URL(req.url()).searchParams.has('mine')),
        page.getByRole('link', {name:'All music', exact:true}).click(),
    ]);
    await page.locator('.collection-tabs a[href="#artists?tab=playlists"]').click();
    await entries.getByText(publicMix.title).waitFor();
    await entries.getByText(owned.title).waitFor();
    assert.deepEqual(errors, []);
    await page.close();
}

async function checkSidebar(browser, mobile) {
    const page = await browser.newPage({ viewport: { width: mobile ? 375 : 1280, height: 800 } });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.route('**/*', route => {
        const url = new URL(route.request().url());
        if (url.hostname === 'sidebar.test' && url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
        if (url.hostname === 'cdn.jsdelivr.net' && url.pathname.includes('alpine')) return route.fulfill({ contentType: 'application/javascript', body: alpine });
        const responses = {
            '/api/player/artists': { items: [], total: 0, has_more: false },
            '/api/player/me': { id: 1, name: 'Listener', role: mobile ? 'user' : 'admin' },
            '/api/player/listening': {plays:0, tracks:0, listened_seconds:0, ranking:[], daily:[]},
            '/api/player/playlists': [likes, owned], '/api/player/playlists/-1': { ...likes, tracks: [] },
            '/api/player/pins': mobile ? [pin] : [],
        };
        return route.fulfill({ json: responses[url.pathname] || [] });
    });
    await page.goto('http://sidebar.test/');
    const menuButton = page.locator('.mobile-library-btn');
    const nav = page.locator(mobile ? 'dialog .music-nav' : '.sidebar-left .music-nav');
    if (mobile) await menuButton.click();
    await nav.getByRole('link', { name: mobile ? pin.title : owned.title }).waitFor();
    if (!mobile) {
        assert.equal(await nav.locator('.shortcut-pin-grip:visible, .shortcut-unpin:visible').count(), 0,
            'automatic shortcuts cannot be moved or unpinned');
    }
    assert.deepEqual(await nav.locator('.music-nav-pages a').allTextContents().then(values => values.map(value => value.trim())),
        ['Catalog', 'Podcasts']);
    await nav.getByRole('link', { name: /Liked songs/ }).click();
    await page.waitForURL('**/#playlist/-1');
    await page.waitForFunction(() => Alpine.store('library').currentPlaylist?.id === -1);
    if (mobile) {
        await page.locator('dialog').waitFor({ state: 'detached' });
        await menuButton.click();
        await page.locator('dialog:modal').waitFor();
        await nav.getByRole('link', { name: /Liked songs/ }).click();
        await page.locator('dialog').waitFor({ state: 'detached' });
        assert.equal(new URL(page.url()).hash, '#playlist/-1');
        await menuButton.click();
        await page.locator('dialog:modal').waitFor();
        await page.keyboard.press('Escape');
        await page.locator('dialog').waitFor({ state: 'detached' });
        assert.ok(await menuButton.evaluate(element => element === document.activeElement));
    } else {
        await page.goto('http://sidebar.test/#artists?tab=playlists');
        await page.waitForFunction(() => Alpine.store('library').view === 'playlists');
        assert.equal(await nav.getByRole('link', { name: 'Catalog' }).getAttribute('aria-current'), 'page');
    }
    const trigger = page.locator(mobile ? '.mobile-account-chip' : '.sidebar-account-trigger');
    const panel = page.locator(mobile ? '#mobile-account-actions' : '#sidebar-account-actions');
    await trigger.click();
    await panel.getByRole('link', {name:'Followed'}).click();
    await page.getByRole('heading', {name:'Followed', exact:true}).waitFor();
    await panel.waitFor({state:'hidden'});
    await trigger.click();
    await panel.getByRole('link', {name:'My listening'}).click();
    await page.waitForURL('**/#listening');
    await panel.waitFor({state:'hidden'});
    await trigger.click();
    assert.equal(await panel.getByRole('link', {name:'My listening'}).getAttribute('aria-current'), 'page');
    await panel.getByRole('button', { name: 'Settings' }).waitFor();
    assert.equal(await panel.getByRole('link', { name: 'Admin Panel' }).count(), mobile ? 0 : 1);
    assert.equal(await panel.getByRole('link', { name: 'Log out' }).getAttribute('href'), '/logout');
    await page.keyboard.press('Escape');
    await panel.waitFor({ state: 'hidden' });
    await trigger.click();
    await panel.getByRole('button', { name: 'Settings' }).click();
    await page.locator('.user-settings-modal').waitFor();
    assert.equal(await panel.isVisible(), false);
    await checkOfflineControls(page, trigger, panel);
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
    assert.deepEqual(errors, []);
    await page.close();
}

async function checkOfflineControls(page, trigger, panel) {
    const modal = page.locator('.user-settings-modal');
    const section = modal.locator('.offline-settings');
    const close = modal.locator('.user-settings-head button');
    const viewport = page.viewportSize();
    if (viewport.width < 600) await page.setViewportSize({...viewport, width:320});
    // Exercise the account/settings UI independently of Service Worker registration on this mock origin.
    await page.evaluate(() => {
        Object.assign(Alpine.store('offline'), {
            supported: true,
            setEnabled(on) { this.enabled = on; },
            setLimit(bytes) { this.limitBytes = bytes; },
            setManual(on) {
                this.manual = this.active = on;
                document.documentElement.classList.toggle('offline-active', on);
            },
        });
    });
    const enable = section.getByRole('switch');
    await page.waitForFunction(() => document.activeElement?.id === 'user-settings-title');
    assert.equal(await enable.getAttribute('aria-checked'), 'false');
    await close.click();
    await page.waitForFunction(selector => document.activeElement?.matches(selector),
        viewport.width < 600 ? '.mobile-account-chip' : '.sidebar-account-trigger');
    await trigger.click();
    await panel.getByText('Offline listening is off').waitFor();
    await panel.getByRole('button', {name:'Manage offline music'}).click();
    await page.waitForFunction(() => document.activeElement?.id === 'offline-settings-title');
    await enable.click();
    await section.getByText('Saved music', {exact:true}).waitFor();
    const range = section.getByRole('slider', {name:'Cache limit'});
    assert.equal(await range.getAttribute('aria-valuetext'), '3 GB');
    const marks = await section.locator('.offline-scale-marks span').evaluateAll(list =>
        list.map(node => ({text:node.textContent, start:node.getBoundingClientRect().left, end:node.getBoundingClientRect().right})));
    assert.deepEqual(marks.map(m => m.text), ['100 MB','1 GB','10 GB','100 GB','512 GB']);
    assert.ok(marks.every((mark, i) => i === 0 || marks[i-1].end <= mark.start),
        'scale labels overlap: ' + JSON.stringify(marks));
    await range.press('Home');
    assert.equal(await range.getAttribute('aria-valuetext'), '100 MB');
    let last = 100e6;
    for (let i = 0; i < 3; i++) {
        await range.press('ArrowRight');
        const next = await page.evaluate(() => Alpine.store('offline').limitBytes);
        assert.ok(next > last, 'each keyboard step must change the stored limit');
        last = next;
    }
    await range.press('End');
    assert.equal(await range.getAttribute('aria-valuetext'), '512 GB');
    await page.evaluate(() => Object.assign(Alpine.store('offline'), {
        bytes:44e6, limitBytes:3e9,
        items:[{id:7,title:'One',artist:'Artist',bytes:40e6},{id:8,title:'Two',artist:'Artist',bytes:4e6}],
    }));
    await section.getByRole('button', {name:'Open cache'}).waitFor();
    assert.equal(await section.getByText('One', {exact:true}).count(), 0, 'settings show the cache summary');
    await close.click();
    await trigger.click();
    const offlineAccount = panel.locator('.offline-account');
    await offlineAccount.getByText('Online', {exact:true}).click();
    assert.ok(await panel.isVisible(), 'clicking status text keeps the account menu open');
    await offlineAccount.click({position:{x:1,y:1}});
    assert.ok(await panel.isVisible(), 'clicking empty space inside the account menu leaves it open');
    const meter = offlineAccount.getByRole('progressbar');
    assert.equal(await meter.getAttribute('aria-valuetext'), '44 MB / 3 GB');
    await offlineAccount.getByText('44 MB / 3 GB',{exact:true}).waitFor();
    assert.equal(await meter.evaluate(el => el.position), 44e6/3e9, 'usage is drawn against the configured limit');
    await meter.click();
    assert.ok(await panel.isVisible(), 'clicking the meter leaves the menu open');
    await page.locator('.content-topbar').click({position:{x:1,y:1}});
    await panel.waitFor({state:'hidden'});
    await trigger.click();
    const manual = panel.getByRole('switch', {name:'Use saved music only'});
    await manual.click();
    assert.equal(await manual.getAttribute('aria-checked'), 'true');
    assert.ok(await panel.isVisible(), 'toggling saved music keeps the menu open');
    assert.equal(await panel.getByRole('link', {name:'My listening'}).isVisible(), false);
    assert.equal(await panel.getByRole('link', {name:'Followed'}).isVisible(), true);
    await panel.getByRole('button', {name:'Settings'}).click();
    await modal.getByText('Devices are unavailable offline.').waitFor();
    await page.keyboard.press('Escape');
    await modal.waitFor({state:'hidden'});
    await page.locator('.device-toggle-btn:visible').click();
    assert.equal((await page.locator('.device-popover:visible').innerText()).trim(), 'Devices are unavailable offline.');
    await page.locator('.device-toggle-btn:visible').click();
    await page.evaluate(() => { Alpine.store('offline').manual = false; });
    await trigger.click();
    await panel.getByText('Offline · automatic').waitFor();
    assert.equal(await manual.getAttribute('aria-checked'), 'false');
    await page.evaluate(() => {
        Alpine.store('user').menuOpen = false;
        Alpine.store('library').view = 'offline_unavailable';
    });
    await page.getByText('This page is unavailable offline.').waitFor();
    assert.equal(await page.getByRole('link', {name:'Open saved catalog'}).getAttribute('href'), '#artists');
}

async function dragShortcut(page, nav, source, destination, touch, cancel = false) {
    const grip = nav.locator('[data-pin-href="' + source.href + '"] .shortcut-pin-grip');
    const row = nav.locator('[data-pin-href="' + destination.href + '"]');
    const handle = await grip.boundingBox(), target = await row.boundingBox();
    assert.ok(handle && target);
    const x = Math.round(handle.x + handle.width / 2);
    const y = Math.round(handle.y + handle.height / 2);
    const end = Math.round(target.y + target.height * .8);
    if (touch) {
        const cdp = await page.context().newCDPSession(page);
        await cdp.send('Input.dispatchTouchEvent', {type:'touchStart',touchPoints:[{x,y}]});
        for (let step = 1; step <= 8; step++) {
            await cdp.send('Input.dispatchTouchEvent', {type:'touchMove',touchPoints:[{x,y:y+(end-y)*step/8}]});
        }
        await cdp.send('Input.dispatchTouchEvent', {type:cancel ? 'touchCancel' : 'touchEnd', touchPoints:[]});
        await cdp.detach();
    } else {
        await page.mouse.move(x,y);
        await page.mouse.down();
        await page.mouse.move(x,end,{steps:8});
        if (cancel) await page.keyboard.press('Escape');
        await page.mouse.up();
    }
}

async function checkPinActions(browser, mobile) {
    const page = await browser.newPage({viewport:{width:mobile ? 375 : 1280,height:800}, hasTouch:mobile, isMobile:mobile});
    const errors = [];
    page.on('pageerror', err => errors.push(err.message));
    await page.clock.install();
    const [a,b,c,d] = ['Low','Hounds of Love','Discovery · '+ 'Anniversary Edition '.repeat(7).trim(),'Kind of Blue'].map((title,index) => (
        {title,href:'#release/'+(index+101),id:index+101,kind:'release'}
    ));
    let stored = [a,b,c,d];
    let rejectMove = false;
    let loseUnpinReply = false;
    let offlineMove = false;
    let offlineRead = false;
    const writes = [];
    await page.route('**/*', route => {
        const {hostname,pathname} = new URL(route.request().url());
        if (hostname === 'sidebar.test' && pathname === '/') return route.fulfill({contentType:'text/html',body:html});
        if (hostname === 'cdn.jsdelivr.net' && pathname.includes('alpine')) return route.fulfill({contentType:'application/javascript',body:alpine});
        if (pathname === '/api/player/pins') {
            const method = route.request().method();
            if (offlineMove && method === 'PUT') {
                offlineMove = false;
                offlineRead = true;
                return route.abort('internetdisconnected');
            }
            if (offlineRead && method === 'GET') {
                offlineRead = false;
                return route.abort('internetdisconnected');
            }
            if (method === 'GET') return route.fulfill({json:stored});
            const change = route.request().postDataJSON();
            writes.push({method,...change});
            if (method === 'PUT' && rejectMove) {
                stored = stored.filter(item => item.href !== c.href);
                return route.fulfill({status:409,json:{error:'Order changed'}});
            }
            const item = [a,b,c,d].find(item => item.href === change.href);
            assert.ok(item, 'only pinned entries can be modified');
            stored = stored.filter(entry => entry.href !== item.href);
            if (method === 'PUT' || change.pinned) {
                const before = stored.findIndex(entry => entry.href === change.before_href);
                const after = stored.findIndex(entry => entry.href === change.after_href);
                stored.splice(before >= 0 ? before : after >= 0 ? after + 1 : stored.length, 0, item);
            }
            if (loseUnpinReply && method === 'POST' && !change.pinned) {
                loseUnpinReply = false;
                return route.abort('failed');
            }
            return route.fulfill({json:stored});
        }
        const responses = {
            '/api/player/artists': {items:[],total:0,has_more:false},
            '/api/player/me': {id:1,name:'Listener'}, '/api/player/playlists':[likes,owned],
            ['/api/player/releases/'+a.id]: {...a,artists:[],tracks:[]},
        };
        return route.fulfill({json:responses[pathname] || []});
    });
    const root = mobile ? 'dialog .music-shortcuts' : '.sidebar-left .music-shortcuts';
    const nav = page.locator(root);
    const menuButton = page.locator('.mobile-library-btn');
    const openDrawer = async () => {
        await menuButton.click();
        await page.locator('dialog:modal').waitFor();
    };
    const row = item => nav.locator('[data-pin-href="' + item.href + '"]');
    const notice = item => page.locator('.pin-undo-notice:visible').filter({hasText:item.title});
    const unpin = async item => {
        await row(item).locator('.shortcut-unpin').click();
        await notice(item).getByRole('button',{name:/Undo/i}).waitFor();
        assert.ok(await notice(item).evaluate(el =>
            el.scrollWidth <= el.clientWidth + 1 &&
            [...el.querySelectorAll('button')].every(button => button.getBoundingClientRect().right <= el.closest('.pin-feedback').getBoundingClientRect().right + 1)
        ), 'Undo remains fully visible for long titles');
    };
    const undo = item => notice(item).getByRole('button',{name:/Undo/i}).click();
    async function expectOrder(expected) {
        const hrefs = expected.map(item => item.href);
        await page.waitForFunction(({root,hrefs}) => {
            const actual = Array.from(document.querySelectorAll(root+' [data-pin-href]'), el => el.dataset.pinHref);
            return !Alpine.store('pins').busy && actual.length === hrefs.length &&
                actual.every((href,index) => href === hrefs[index]);
        }, {root,hrefs});
        assert.deepEqual(stored.map(item => item.href), hrefs);
    }
    await page.goto('http://sidebar.test/');
    if (mobile) await openDrawer();
    await expectOrder([a,b,c,d]);
    const initialHash = new URL(page.url()).hash;
    await dragShortcut(page,nav,a,c,mobile,true);
    await expectOrder([a,b,c,d]);
    assert.equal(writes.length, 0, 'a cancelled drag leaves the order intact');
    await dragShortcut(page,nav,a,c,mobile);
    await expectOrder([b,c,a,d]);
    assert.equal(new URL(page.url()).hash, initialHash, 'dragging a pin cannot navigate');
    assert.deepEqual(writes.map(({method,href,before_href})=>[method,href,before_href]),[['PUT',a.href,d.href]]);
    await page.reload();
    if (mobile) await openDrawer();
    await expectOrder([b,c,a,d]);
    const grip = row(b).locator('.shortcut-pin-grip');
    await grip.focus();
    await page.keyboard.press('ArrowDown');
    await expectOrder([c,b,a,d]);
    assert.ok(await grip.evaluate(button => document.activeElement === button), 'keyboard reordering retains focus');

    // Remove adjacent head entries; the first undo no longer has its old next neighbour.
    await unpin(c);
    await unpin(b);
    await expectOrder([a,d]);
    if (mobile) {
        await page.keyboard.press('Escape');
        await page.locator('dialog').waitFor({state:'detached'});
    }
    await undo(c);
    if (mobile) await openDrawer();
    await expectOrder([c,a,d]);
    await undo(b);
    await expectOrder([c,b,a,d]);

    await unpin(a);
    await unpin(d);
    await expectOrder([c,b]);
    const priorWrites = writes.length;
    await notice(d).getByRole('button',{name:'Close · '+d.title,exact:true}).click();
    await notice(d).waitFor({state:'hidden'});
    assert.ok(await notice(a).isVisible(), 'dismissing one unpin notice preserves the other');
    await expectOrder([c,b]);
    assert.equal(writes.length,priorWrites,'dismissing a notice leaves the stored arrangement alone');
    await undo(a);
    await expectOrder([c,b,a]);
    await page.evaluate(item=>Alpine.store('pins').toggle(item),d);
    await expectOrder([c,b,a,d]);

    loseUnpinReply = true;
    await unpin(d);
    await page.clock.fastForward(8000);
    assert.ok(await notice(d).isVisible(), 'undo remains available before ten seconds');
    await page.clock.fastForward(2200);
    await notice(d).waitFor({state:'hidden'});
    await expectOrder([c,b,a]);
    if (!mobile) {
        offlineMove = true;
        await row(b).locator('.shortcut-pin-grip').focus();
        await Promise.all([
            page.waitForEvent('requestfailed', req => req.url().endsWith('/api/player/pins') && req.method() === 'GET'),
            page.keyboard.press('ArrowUp'),
        ]);
        await expectOrder([c,b,a]);
        await page.locator('.pin-action-error:visible').waitFor();
        rejectMove = true;
        await row(b).locator('.shortcut-pin-grip').focus();
        await page.keyboard.press('ArrowUp');
        await expectOrder([b,a]);
        await unpin(b);
        await unpin(a);
        await expectOrder([]);
        assert.equal(await nav.getByRole('link',{name:owned.title}).count(), 0);
        await page.clock.fastForward(10_100);
        await nav.getByRole('link',{name:owned.title}).waitFor();
        assert.equal(await nav.locator('.shortcut-unpin:visible, .shortcut-pin-grip:visible').count(), 0);
    } else {
        await row(a).getByRole('link').tap();
        await page.waitForURL('**/'+a.href);
        await page.locator('dialog').waitFor({state:'detached'});
    }
    assert.deepEqual(errors, []);
    await page.close();
}

(async () => {
    const browser = await chromium.launch({
        headless: true, ...(process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : {}),
    });
    try {
        await checkSidebar(browser, false);
        await checkSidebar(browser, true);
        await checkCollection(browser);
        await checkPinActions(browser, false);
        await checkPinActions(browser, true);
        console.log('Desktop and mobile sidebar navigation, offline storage controls, pin reordering and undo passed.');
    } finally {
        await browser.close();
    }
})().catch(error => { console.error(error); process.exitCode = 1; });

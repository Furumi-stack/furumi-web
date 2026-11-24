/* global OfflineCatalog */
importScripts('/offline-catalog.js');

const CACHE = 'furumusic-player-shell-v1';
const SETTINGS = '/__furumusic_offline__/settings';
const SHELL = '/__furumusic_offline__/player';
const ASSETS = ['/player.webmanifest', '/player-icon.svg'];
const ACCOUNT = 'X-Furumusic-Player-Account';
const ORIGIN = self.location.origin;
const BINDINGS = '/__furumusic_offline__/client/';
const bound = new Map();
let statePromise;
let serial = Promise.resolve();
let network = new AbortController();
let repo;

function catalog(account) {
    if (!repo || String(repo.account) !== account) repo = new OfflineCatalog(account);
    return repo;
}

function exclusive(work) {
    const operation = serial.then(work);
    serial = operation.catch(() => {});
    return operation;
}

function settings() {
    if (!statePromise) {
        statePromise = caches.open(CACHE).then(cache => cache.match(SETTINGS)).then(r => r ? r.json() : null);
    }
    return statePromise;
}

async function broadcast(type, state) {
    const windows = await self.clients.matchAll({ type:'window', includeUncontrolled:true });
    for (const client of windows) {
        if (isPlayer(client.url)) client.postMessage({ type, account:state.account, mode:state.mode });
    }
}

async function save(state) {
    const cache = await caches.open(CACHE);
    if (state) await cache.put(SETTINGS, Response.json(state));
    else await cache.delete(SETTINGS);
    statePromise = Promise.resolve(state);
    network.abort();
    network = new AbortController();
    if (state) await broadcast('offline-mode', state);
}

async function forget(state) {
    if (!state) return;
    const cache = await caches.open(CACHE);
    await cache.delete(SHELL);
    await save(null);
    // Notify pages before waiting for the catalog write lock. Downloads can hold
    // that lock while their authorization response is still pending.
    await broadcast('offline-purge', { account:state.account, mode:'disabled' });
    await catalog(state.account).clear();
    repo = undefined;
}

function isPlayer(url) {
    if (!url) return false;
    const loc = new URL(url);
    return loc.origin === ORIGIN && loc.pathname === '/';
}

async function bind(id, account) {
    if (!id || bound.get(id) === account) return;
    bound.set(id, account);
    await (await caches.open(CACHE)).put(BINDINGS + id, new Response(account));
}

async function accountFor(id) {
    if (!id) return null;
    if (!bound.has(id)) {
        const response = await (await caches.open(CACHE)).match(BINDINGS + id);
        if (response) bound.set(id, await response.text());
    }
    return bound.get(id);
}

async function cleanBindings(keep = '') {
    const cache = await caches.open(CACHE);
    const clients = new Set((await self.clients.matchAll({type:'window', includeUncontrolled:true})).map(c => c.id));
    for (const request of await cache.keys()) {
        const pathname = new URL(request.url).pathname;
        if (!pathname.startsWith(BINDINGS)) continue;
        const id = pathname.slice(BINDINGS.length);
        if (id !== keep && !clients.has(id)) { await cache.delete(request); bound.delete(id); }
    }
}

function isOffline(state) {
    return state?.mode === 'manual' || state?.mode === 'auto';
}

function validShell(response, account) {
    return response.status === 200 && response.headers.get(ACCOUNT) === account &&
        response.headers.get('Content-Type')?.startsWith('text/html');
}

function probeAccount(response) {
    const id = response.headers.get(ACCOUNT);
    if (response.status === 204 && /^[1-9]\d*$/.test(id)) return id;
    return response.status === 401 && id === '0' ? id : null;
}

async function verifySession(request) {
    const signal = requestSignal(request, 3500);
    try {
        const response = await fetch('/offline-probe', {
            credentials:'same-origin', cache:'no-store', redirect:'manual', signal:signal.signal,
        });
        return probeAccount(response);
    } catch {
        return null;
    } finally {
        signal.clear();
    }
}

async function configure({ account, mode }, sourceId) {
    account = String(account);
    if (!/^[1-9]\d*$/.test(account) || !['manual','auto','online','disabled'].includes(mode)) {
        throw Error('Invalid offline settings');
    }
    const current = await settings();
    const cache = await caches.open(CACHE);
    const caller = await accountFor(sourceId);
    if ((caller && caller !== account) || (isOffline(current) && current.account !== account)) {
        throw Error('Account changed');
    }
    if (mode === 'disabled') {
        if (current?.account === account) {
            await cache.delete(SHELL);
            await save({ account, mode });
        }
        return mode;
    }
    if (current?.account !== account || current.mode === 'disabled' || !await cache.match(SHELL)) {
        if (mode !== 'online') throw Error('Save the player online first');
        const shell = await fetch('/', {cache:'no-store', credentials:'same-origin', redirect:'manual'});
        if (!validShell(shell, account)) throw Error('Could not verify this account');
        if (current && current.account !== account) await forget(current);
        await cache.addAll(ASSETS);
        await cache.put(SHELL, shell);
    }
    if (current?.account !== account || current.mode !== mode) await save({ account, mode });
    await bind(sourceId, account);
    return mode;
}

self.addEventListener('install', event => event.waitUntil(self.skipWaiting()));
self.addEventListener('activate', event => event.waitUntil(cleanBindings().then(() => self.clients.claim())));
self.addEventListener('message', event => {
    if (!isPlayer(event.source?.url)) return;
    if (event.data?.type === 'offline-client' && /^[1-9]\d*$/.test(String(event.data.account))) {
        event.waitUntil(bind(event.source.id, String(event.data.account)));
        return;
    }
    if (event.data?.type !== 'offline-config') return;
    const reply = event.ports[0];
    event.waitUntil(exclusive(() => configure(event.data, event.source.id))
        .then(mode => { reply?.postMessage({ ok:true, mode }); },
              e => { reply?.postMessage({ ok:false, error:e.message }); }));
});

function unavailable() {
    return Response.json({ error:'Unavailable offline' }, { status:503, headers:{'Cache-Control':'no-store'} });
}

async function local(request, state, rootNavigation) {
    const url = new URL(request.url);
    if (url.origin !== ORIGIN) return Response.error();
    const cache = await caches.open(CACHE);
    if (rootNavigation) {
        const shell = await cache.match(SHELL);
        if (!shell || !validShell(shell, state.account)) return unavailable();
        const html = (await shell.text()).replace('data-offline-mode="online"', 'data-offline-mode="' + state.mode + '"');
        return new Response(html, {headers:{'Content-Type':'text/html; charset=utf-8', 'Cache-Control':'private, no-store', [ACCOUNT]:state.account}});
    }
    if (ASSETS.includes(url.pathname)) return await cache.match(url.pathname) || unavailable();
    if (request.mode === 'navigate') return unavailable();
    const read = ['GET','HEAD'].includes(request.method) ||
        (request.method === 'POST' && url.pathname === '/api/player/tracks-by-ids');
    if (read && url.pathname.startsWith('/api/')) {
        try {
            return await catalog(state.account).respond(request) || unavailable();
        } catch {
            return unavailable();
        }
    }
    return unavailable();
}

// A mode change cuts off requests already waiting for headers and in-flight audio streams.
function requestSignal(request, timeout) {
    const controller = new AbortController();
    const signals = [network.signal, request.signal];
    let signal;
    const abort = () => controller.abort();
    if (AbortSignal.any) {
        signal = AbortSignal.any([...signals, controller.signal]);
    } else {
        signal = controller.signal;
        for (const source of signals) {
            if (source.aborted) controller.abort();
            else source.addEventListener('abort', abort, { once:true });
        }
    }
    const timer = timeout ? setTimeout(() => controller.abort(), timeout) : null;
    return { signal, clear() {
        clearTimeout(timer);
        if (!AbortSignal.any) for (const source of signals) source.removeEventListener('abort', abort);
    } };
}

function fallback(request, state, rootNavigation) {
    return exclusive(async () => {
        const now = await settings();
        if (now === state && state.mode === 'online') await save({account:state.account, mode:'auto'});
        return settings();
    }).then(now => now?.account === state.account && isOffline(now)
        ? local(request, now, rootNavigation) : unavailable());
}

async function handle(event) {
    const {request} = event;
    const url = new URL(request.url);
    const sameOrigin = url.origin === ORIGIN;
    const rootNavigation = request.mode === 'navigate' && isPlayer(request.url);
    const auth = sameOrigin && (url.pathname === '/logout' || url.pathname === '/auth/oidc/callback' ||
        (url.pathname === '/login' && request.method === 'POST'));
    const state = await settings();
    if (!state) return fetch(request);
    const source = event.clientId && await self.clients.get(event.clientId);
    const player = rootNavigation || isPlayer(source?.url) || (!source && isPlayer(request.referrer));
    if (rootNavigation) event.waitUntil(cleanBindings(event.resultingClientId));
    if (isOffline(state) && sameOrigin && ASSETS.includes(url.pathname)) return local(request, state, false);
    if (auth) {
        if (player && isOffline(state)) return unavailable();
        let response;
        try {
            response = await fetch(request);
            return response;
        } finally {
            let changed = url.pathname === '/logout';
            if (!changed && response && response.status < 400) {
                const identity = await verifySession(request);
                // A rejected login also returns 200. Keep existing downloads when
                // the session still belongs to their owner.
                changed = identity === null ? response.type === 'opaqueredirect' || response.redirected : identity !== state.account;
            }
            if (changed) {
                await exclusive(async () => { if ((await settings())?.account === state.account) await forget(state); });
            }
        }
    }
    if (!player) return fetch(request);
    if (state.mode === 'disabled' && !rootNavigation) return fetch(request);
    if (!rootNavigation && source && (await accountFor(source.id)) !== state.account) return unavailable();
    const probe = sameOrigin && url.pathname === '/offline-probe' && request.method === 'GET';
    if (isOffline(state) && !(probe && state.mode === 'auto')) {
        if (rootNavigation) await bind(event.resultingClientId, state.account);
        return local(request, state, rootNavigation);
    }
    const savedAudio = /^\/api\/player\/(?:stream\/\d+|federation\/cache\/[a-f\d]{64})$/.test(url.pathname);
    if (state.mode === 'online' && sameOrigin && ['GET','HEAD'].includes(request.method) &&
        savedAudio && url.searchParams.get('saved') === '1') {
        const saved = await catalog(state.account).respond(request).catch(() => null);
        if (saved && (saved.ok || saved.status === 416)) return saved;
    }

    const timed = state.mode !== 'disabled' && (rootNavigation || probe || url.pathname === '/api/player/me');
    const signal = requestSignal(request, timed ? 3500 : 0);
    let response;
    try {
        // The queue lookup is a read with a JSON body; retain it for a local retry.
        const queueLookup = sameOrigin && request.method === 'POST' && url.pathname === '/api/player/tracks-by-ids';
        response = await fetch(queueLookup ? request.clone() : request, {signal:signal.signal});
    } catch {
        if (request.signal.aborted || state.mode === 'disabled' || probe || !sameOrigin) return Response.error();
        if (rootNavigation) await bind(event.resultingClientId, state.account);
        return fallback(request, state, rootNavigation);
    } finally {
        signal.clear();
    }
    if (([502,503,504].includes(response.status) || (rootNavigation && response.status >= 500)) &&
        !probe && state.mode === 'online' && sameOrigin) {
        if (rootNavigation) await bind(event.resultingClientId, state.account);
        return fallback(request, state, rootNavigation);
    }
    // Intermediaries and older API handlers may return a login page/401 during
    // an outage. Only our session endpoint or a labeled player page identifies an account.
    const checkAuth = sameOrigin && (response.status === 401 ||
        (rootNavigation && (response.type === 'opaqueredirect' || response.redirected ||
            (response.ok && !validShell(response, state.account)))) ||
        (probe && probeAccount(response) !== state.account));
    if (checkAuth) {
        const label = rootNavigation && response.ok ? response.headers.get(ACCOUNT) : null;
        const identity = probe ? probeAccount(response) : /^\d+$/.test(label) ? label : await verifySession(request);
        if (identity === state.account && !rootNavigation) return response;
        if (identity === null || identity === state.account) {
            if (probe) return unavailable();
            if (state.mode === 'disabled') return response;
            if (rootNavigation) await bind(event.resultingClientId, state.account);
            return fallback(request, state, rootNavigation);
        }
        // Revocation aborts the previous account's streams. Detach this verified
        // response first, so the login redirect or new account page still reaches the window.
        const reply = response.type === 'opaqueredirect' ? Response.redirect(ORIGIN + '/login', 302) :
            new Response(response.body && ![204,205,304].includes(response.status) && request.method !== 'HEAD' ? await response.blob() : null, {
                headers:response.headers, status:response.status, statusText:response.statusText,
            });
        await exclusive(async () => { if ((await settings())?.account === state.account) await forget(state); });
        return reply;
    }
    if (rootNavigation && state.mode === 'online' && validShell(response, state.account)) {
        await bind(event.resultingClientId, state.account);
        const copy = response.clone();
        event.waitUntil(exclusive(async () => {
            if (await settings() === state) await (await caches.open(CACHE)).put(SHELL, copy);
        }));
    }
    return response;
}

self.addEventListener('fetch', event => {
    if (['https:','http:'].includes(new URL(event.request.url).protocol)) event.respondWith(handle(event));
});

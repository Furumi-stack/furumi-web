const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { test } = require('node:test');
const vm = require('node:vm');

const template = readFileSync(`${__dirname}/../templates/admin/v2.html`, 'utf8');
const script = template.match(/<script>\s*(function adminV2\(\)[\s\S]*?)<\/script>/)[1];

function createApp() {
    const context = vm.createContext({});
    vm.runInContext(script, context);
    const app = context.adminV2();
    app.showToast = message => { app.toastMessage = message; };
    app.loadReviews = async () => { app.reviewsReloaded = true; };
    app.loadLibrary = async () => { app.libraryReloaded = true; };
    return app;
}

test('approves individually selected reviews and reports partial failures', async () => {
    const app = createApp();
    app.toggleReview({ id: 12, status: 'pending' });
    app.toggleReview({ id: 34, status: 'pending' });
    app.toggleReview({ id: 56, status: 'failed' });
    assert.equal(app.selectedPendingReviewCount(), 2);
    app.request = async (url, options) => {
        assert.equal(url, '/admin/v2/api/reviews/bulk');
        assert.equal(options.method, 'POST');
        assert.deepEqual(JSON.parse(options.body), {
            action: 'approve', mode: 'ids', ids: [12, 34, 56],
            filter: { status: null, search: null }
        });
        assert.equal(app.reviewBulkBusy, true);
        return { ok: false, affected: 1, failed: 1, skipped: 1 };
    };
    await app.bulkReviews('approve');
    assert.equal(app.toastMessage, '1 review(s) approved · 1 failed · 1 skipped');
    assert.equal(app.selectedReviewCount(), 0);
    assert.equal(app.reviewsReloaded, true);
    assert.equal(app.libraryReloaded, true);
    assert.equal(app.reviewBulkBusy, false);
});

test('select all approves the whole searched filter across pages', async () => {
    const app = createApp();
    app.reviewFilter = { status: 'pending', search: 'Album' };
    app.reviews = { total: 320, items: [{ id: 1, status: 'pending' }] };
    app.selectReviewFilter();
    assert.equal(app.selectedPendingReviewCount(), 320);
    app.request = async (_url, options) => {
        assert.deepEqual(JSON.parse(options.body), {
            action: 'approve', mode: 'filter', ids: [],
            filter: { status: 'pending', search: 'Album' }
        });
        return { affected: 320, failed: 0, skipped: 0 };
    };
    await app.bulkReviews('approve');
    assert.equal(app.toastMessage, '320 review(s) approved · 0 failed · 0 skipped');
});

test('mixed status filter enables approval only when pending reviews exist', () => {
    const app = createApp();
    app.reviews = { total: 10, status_counts: [{ status: 'pending', count: 3 }, { status: 'queued', count: 7 }] };
    app.selectReviewFilter();
    assert.equal(app.selectedPendingReviewCount(), 3);
    app.reviewFilter.status = 'queued';
    assert.equal(app.selectedPendingReviewCount(), 0);
});

test('empty and non-pending selections do not submit approval', async () => {
    const app = createApp();
    app.request = async () => { assert.fail('unexpected request'); };
    await app.bulkReviews('approve');
    app.toggleReview({ id: 1, status: 'queued' });
    await app.bulkReviews('approve');
});

test('blocks duplicate bulk submissions while approval is running', async () => {
    const app = createApp();
    app.toggleReview({ id: 1, status: 'pending' });
    let finish;
    let calls = 0;
    app.request = async () => {
        calls += 1;
        return new Promise(resolve => { finish = resolve; });
    };
    const approval = app.bulkReviews('approve');
    await app.bulkReviews('approve');
    await app.bulkReviews('requeue');
    assert.equal(calls, 1);
    finish({ affected: 1 });
    await approval;
    assert.equal(app.reviewBulkBusy, false);
});

test('request failure retains selection and restores bulk controls', async () => {
    const app = createApp();
    app.toggleReview({ id: 1, status: 'pending' });
    app.request = async () => { throw new Error('Connection lost'); };
    await app.bulkReviews('approve');
    assert.equal(app.selectedReviewCount(), 1);
    assert.equal(app.toastMessage, 'Connection lost');
    assert.equal(app.reviewBulkBusy, false);
    assert.equal(app.reviewBulkAction, null);
});

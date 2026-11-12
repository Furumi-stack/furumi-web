const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {test} = require('node:test');
const vm = require('node:vm');
const template = readFileSync(`${__dirname}/../templates/admin/v2.html`, 'utf8');
const script = template.match(/<script>\s*(function adminV2\(\)[\s\S]*?)<\/script>/)[1];
function app() {
    const context = vm.createContext({URLSearchParams, Date}); vm.runInContext(script, context);
    const a = context.adminV2(); a.icons = () => {}; a.setRoute = () => {}; return a;
}
test('artist → listeners → user retains artist and period filters', async () => {
    const a = app(); const urls=[];
    a.request = async url => { urls.push(new URL(url, 'http://localhost').searchParams); return {}; };
    a.statisticsFilter.from = '2026-11-01'; a.statisticsHistoryPage = 5;
    await a.statisticsSelectArtist('Artist & Friends');
    assert.equal(urls[0].get('kind'), 'users'); assert.equal(urls[0].get('artist'), 'Artist & Friends');
    assert.equal(urls[0].get('history_page'), '1');
    await a.statisticsExplore({key:'23', label:'Alice'});
    assert.equal(urls[1].get('user_id'), '23'); assert.equal(urls[1].get('artist'), 'Artist & Friends');
    assert.equal(urls[1].get('from'), '2026-11-01');
    await a.statisticsClearFilter('user_id'); assert.equal(urls[2].get('user_id'), null);
    assert.equal(urls[2].get('artist'), 'Artist & Friends');
});
test('latest filters win when earlier requests arrive late', async () => {
    const a = app(); const responses=[];
    a.request = () => new Promise(resolve => responses.push(resolve));
    const first = a.loadStatistics(); const second = a.statisticsSelectArtist('Second');
    responses[1]({marker:2}); await second; responses[0]({marker:1}); await first;
    assert.equal(a.statisticsData.marker, 2); assert.equal(a.statisticsLoading, false);
});
test('errors are shown separately from an empty result', async () => {
    const a = app(); a.request = async () => {throw new Error('Database unavailable')};
    await a.loadStatistics(); assert.equal(a.statisticsError, 'Database unavailable'); assert.equal(a.statisticsLoading, false);
});
test('opening a user starts full history without an earlier artist selection', () => {
    const a = app(); a.statisticsFilter.artist = 'Previous'; a.statisticsSearch = 'old search'; a.statisticsHistoryKind = 'short'; a.statisticsFilter.source = 'synced'; a.statisticsSetPeriod = () => {};
    a.openStatistics({id:4, username:'user4'});
    assert.equal(a.statisticsFilter.user_id, 4); assert.equal(a.statisticsFilter.artist, '');
    assert.equal(a.statisticsSearch, ''); assert.equal(a.statisticsHistoryKind, 'all'); assert.equal(a.statisticsFilter.source, 'all');
    assert.equal(a.statisticsPeriod, 'all'); assert.equal(a.activeView, 'statistics'); assert.equal(a.userModalOpen, false);
});

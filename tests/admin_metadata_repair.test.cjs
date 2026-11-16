const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { test } = require('node:test');
const vm = require('node:vm');
const template = readFileSync(`${__dirname}/../templates/admin/v2.html`, 'utf8');
const script = template.match(/<script>\s*(function adminV2\(\)[\s\S]*?)<\/script>/)[1];
function createApp() {
    const context = vm.createContext({URLSearchParams});
    vm.runInContext(script, context);
    const app = context.adminV2();
    app.showToast = message => { app.toastMessage = message; };
    app.$nextTick = fn => fn();
    app.$refs = {};
    app.repairSelected = [{kind:'artists',id:1,title:'Artist'}];
    app.repairExpanded = {snapshot:{artists:[{id:1,name:'Artist'}],releases:[{id:2,title:'#ИЗИРОК',year:2018}],tracks:[{id:3,title:'Actual song',release_id:2}]}};
    return app;
}
const emptyPlan = () => ({artists:[],releases:[],tracks:[],questions:[],delete:{artists:[],releases:[],tracks:[]}});

test('deselect all retains related rows and allows selecting only the needed resources', async () => {
    const app = createApp();
    app.setAllRepairIncluded(false);
    assert.equal(app.repairIncludedCount(),0);
    assert.equal(app.repairRowsCount(),3);
    app.setRepairIncluded('releases',2,true);
    app.setRepairIncluded('tracks',3,true);
    assert.equal(app.repairIncludedCount(),2);
    app.repairDraft = 'Only these';
    app.request = async (url,options) => {
        assert.ok(url.endsWith('/plan'), 'must not re-expand exclusions');
        const body = JSON.parse(options.body);
        assert.deepEqual(body.selection,{artists:[],releases:[2],tracks:[3]});
        return {plan:emptyPlan(),snapshot:app.repairExpanded.snapshot,message:'Done'};
    };
    await app.sendRepair();
    assert.equal(app.repairChat.length,2);
    app.setAllRepairIncluded(true);
    assert.equal(app.repairIncludedCount(),3);
});

test('checking only releases includes their contents without selecting the artist', () => {
    const app = createApp();
    app.setAllRepairIncluded(false);
    app.setRepairIncluded('releases',2,true);
    assert.deepEqual(JSON.parse(JSON.stringify(app.repairSelection(true))),{artists:[],releases:[2],tracks:[3]});
    assert.equal(app.repairIsIncluded('tracks',3),true);
    assert.equal(app.repairIsIncluded('artists',1),false);
    app.setRepairIncluded('tracks',3,false);
    assert.equal(app.repairIsIncluded('tracks',3),false);
    assert.deepEqual(JSON.parse(JSON.stringify(app.repairSelection(true))).tracks,[]);
    assert.deepEqual(JSON.parse(JSON.stringify(app.repairExcludedTracks)),[3]);
    app.setAllRepairIncluded(false);
    app.setRepairIncluded('releases',2,true);
    assert.equal(app.repairIsIncluded('tracks',3),true);
});

test('explicit track exclusions are sent to planning without re-expanding them', async () => {
    const app=createApp();
    app.setAllRepairIncluded(false);
    app.setRepairIncluded('releases',2,true);
    app.setRepairIncluded('tracks',3,false);
    app.repairDraft='Edit release only';
    app.request=async(url,options)=>{
        assert.ok(url.endsWith('/plan'));
        const body=JSON.parse(options.body);
        assert.deepEqual(body.selection,{artists:[],releases:[2],tracks:[]});
        assert.deepEqual(body.excluded_track_ids,[3]);
        return {plan:emptyPlan(),snapshot:app.repairExpanded.snapshot,message:'Done'};
    };
    await app.sendRepair();
});

test('clarification replaces the single plan including removed changes and questions', async () => {
    const app = createApp();
    app.repairPlan = {...emptyPlan(),delete:{artists:[],releases:[],tracks:[3]},questions:['Which release?']};
    app.repairChat = [{role:'user',content:'Delete song'},{role:'assistant',content:'Please review'}];
    app.repairPlanOpen = true;
    app.repairDraft = 'Keep song, cancel everything';
    app.request = async (_url,options) => {
        const body=JSON.parse(options.body);
        assert.deepEqual(body.current_plan.delete.tracks,[3]);
        assert.equal(body.conversation.length,2);
        return {plan:emptyPlan(),snapshot:app.repairExpanded.snapshot,message:'Cancelled'};
    };
    await app.sendRepair();
    assert.equal(app.repairChangeCount(),0);
    assert.equal(app.repairPlan.questions.length,0);
    assert.equal(app.repairPlanOpen,true);
    assert.equal(app.repairChat.length,4);
});

test('failed revision preserves prior plan and retry avoids duplicate user messages', async () => {
    const app = createApp();
    const prior = {...emptyPlan(),delete:{artists:[],releases:[],tracks:[3]}};
    app.repairPlan = prior;
    app.repairDraft = 'Keep song';
    app.request = async () => {throw Error('Offline');};
    await app.sendRepair();
    assert.equal(app.repairPlan,prior);
    assert.equal(app.repairIncomplete,true);
    assert.equal(app.repairChat.length,1);
    app.request = async (_url,options) => {
        assert.equal(JSON.parse(options.body).conversation.length,0);
        return {plan:emptyPlan(),snapshot:app.repairExpanded.snapshot,message:'Kept'};
    };
    await app.sendRepair();
    assert.equal(app.repairChat.length,2);
    assert.equal(app.repairIncomplete,false);
});

test('incomplete plan cannot apply, deletion-only plan can apply and clears stale selection', async () => {
    const app = createApp();
    app.repairPlan = {...emptyPlan(),delete:{artists:[],releases:[],tracks:[3]}};
    app.repairSnapshot = app.repairExpanded.snapshot;
    app.repairIncomplete = true;
    app.request = async () => assert.fail('must not apply incomplete plan');
    await app.applyRepair();
    app.repairIncomplete = false;
    app.request = async (url,options) => {
        assert.ok(url.endsWith('/apply'));
        assert.deepEqual(JSON.parse(options.body).plan.delete.tracks,[3]);
        return {history_id:7};
    };
    app.loadRepairHistory = async () => {};
    app.loadLibrary = async () => {};
    await app.applyRepair();
    assert.equal(app.repairIncludedCount(),0);
    assert.equal(app.repairPlan,null);
    assert.equal(app.repairChat.at(-1).content.includes('#7'),true);
});

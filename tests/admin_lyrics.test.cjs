const assert=require('node:assert/strict'),{test}=require('node:test'),fs=require('node:fs'),vm=require('node:vm');
const template=fs.readFileSync(`${__dirname}/../templates/admin/v2.html`,'utf8');
const ui=fs.readFileSync(`${__dirname}/../templates/admin/ui.html`,'utf8').match(/<script>([\s\S]*?)<\/script>/)[1];
const script=ui+'\n'+template.match(/<script>\s*(function adminV2\(\)[\s\S]*?)<\/script>/)[1];
function setup(){
 const context=vm.createContext({crypto:{randomUUID:()=> 'fixture-uuid'},setTimeout(){}});vm.runInContext(script,context);
 const app=context.adminV2();app.icons=()=>{};app.showToast=()=>{};app.refreshCountsOnly=async()=>{};
 app.activeLibraryItem={kind:'tracks',id:1};app.editorDetail={};app.editorDraft={title:'Song',hidden:'false',release_id:2,artist_ids:[]};app.editorCanSave=()=>true;
 return app;
}
test('track save includes alternative lyrics and retry, preserving untouched provider text',async()=>{
 const app=setup(),requests=[];app.editorLyrics=[{id:'lrclib:1',label:'Original',source:'lrclib',source_id:'1',language:null,plain_text:' words\n',synced_lrc:'[00:01]words\n',instrumental:false}];app.editorLyricsRetry=true;
 app.request=async(url,options)=>{requests.push(JSON.parse(options.body));return {id:1,kind:'tracks',title:'Song'};};
 app.replaceLibraryItem=()=>{};app.loadEditorDetail=async()=>{};
 await app.saveLibraryItem();
 assert.equal(requests[0].lyrics.versions[0].plain_text,' words\n');
 assert.equal(requests[0].lyrics.versions[0].source,'lrclib');
 assert.equal(requests[0].lyrics.retry_lookup,true);
});
test('manual alternatives have unique local identities and a bounded count',()=>{
 const app=setup();app.addLyricsVersion();
 assert.equal(app.editorLyrics[0].id,'manual:fixture-uuid');assert.equal(app.editorLyrics[0].source,'manual');
 app.editorLyrics=Array.from({length:16},(_,i)=>({id:String(i)}));app.addLyricsVersion();assert.equal(app.editorLyrics.length,16);
});

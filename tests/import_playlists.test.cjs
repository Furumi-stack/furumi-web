const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {test} = require('node:test');
const vm = require('node:vm');
const {randomUUID} = require('node:crypto');
const template = readFileSync(`${__dirname}/../templates/player/scripts.html`, 'utf8');
function downloads(fetchImpl) {
    let code = template.slice(template.indexOf("    Alpine.store('torrents', {"), template.indexOf("    Alpine.store('playlists', {"));
    code = code.replace(/\{\{[^}]+\}\}/g,'true').replace(/sourceTab: .*?,\n/,"sourceTab: 'files',\n");
    const stores={playlists:{list:[{id:10,title:'Mine',kind:'user',is_own:true},{id:20,title:'Other',kind:'user',is_own:false}],reload:async()=>{}}};
    const requests=[];
    class XHR {
        headers={}; upload={}; status=200; responseText='{}';
        open(method,url) {this.url=url;}
        setRequestHeader(key,value) {this.headers[key]=value;}
        send(file) {requests.push({file,headers:this.headers});this.onload();}
    }
    const context=vm.createContext({Alpine:{store:(name,value)=>value?(stores[name]=value):stores[name]},fetch:fetchImpl,
        T:new Proxy({}, {get:(_,key)=>key}),crypto:{randomUUID},XMLHttpRequest:XHR,redirectOnUnauthorized:()=>{},console,setInterval:()=>0,clearInterval:()=>{}});
    vm.runInContext(code,context);
    const store=stores.torrents;
    store._poll=()=>{};store.loadSessions=async()=>{};store.loadAgentStatus=async()=>{};store.loadLocalUploadHistory=async()=>{};
    return {store,requests};
}
test('destinations are independent, owned playlists only, new titles required',()=>{
    const {store:s}=downloads();
    assert.deepEqual(Array.from(s.youtubeOwnedPlaylists(),p=>p.id),[10]);
    s.localPlaylistChoice='__new__';assert.equal(s.importDestinationValid('local'),false);
    assert.equal(s.importDestinationValid('torrent'),true);
    s.localFiles=[{name:'song.flac',webkitRelativePath:'Soundtrack/song.flac'}];s.importPlaylistChoiceChanged('local');
    assert.equal(s.localNewPlaylistTitle,'Soundtrack');assert.equal(s.importDestinationValid('local'),true);
});
test('failed torrent starts reuse the newly created playlist and send only selected files',async()=>{
    const calls=[];
    const {store:s}=downloads(async(url,opts)=>{
        calls.push({url,data:JSON.parse(opts.body)});
        return url==='/api/player/playlists'?{ok:true,json:async()=>({id:33,title:'Soundtrack',kind:'user',is_own:true})}:{ok:false,json:async()=>({error:'retry'})};
    });
    s.previewData={id:'torrent-1'};s.selected=new Set([1,3]);s.torrentPlaylistChoice='__new__';s.torrentNewPlaylistTitle='Soundtrack';
    await s.start();await s.start();
    assert.equal(calls.filter(c=>c.url==='/api/player/playlists').length,1);
    assert.deepEqual(calls[1].data,{selected_files:[1,3],target_playlist_id:33});
    assert.deepEqual(calls[2].data,calls[1].data);assert.equal(s.loading,false);
});
test('local files carry the same playlist destination and retries retain upload IDs',async()=>{
    const {store:s,requests}=downloads();const f={name:'a.flac',size:10};
    s.setLocalFiles([f]);s.localTargetPlaylistId=10;
    await s.uploadLocalFile(f,0,10);await s.uploadLocalFile(f,0,10);
    assert.equal(requests[0].headers['X-Furumusic-Target-Playlist-Id'],'10');
    assert.equal(requests[0].headers['X-Furumusic-Upload-Id'],requests[1].headers['X-Furumusic-Upload-Id']);
    s.clearLocalFiles();s.setLocalFiles([f]);await s.uploadLocalFile(f,0,10);
    assert.equal(requests[2].headers['X-Furumusic-Target-Playlist-Id'],undefined);
});
test('partially failed batches keep their destination, create one playlist and retry remaining files',async()=>{
    let creates=0;let attempts=0;
    const {store:s}=downloads(async(url)=>{
        if(url==='/api/player/playlists') {creates++;return {ok:true,json:async()=>({id:44})};}
        return {ok:true};
    });
    s.setLocalFiles([{name:'a.mp3',size:1},{name:'b.mp3',size:1}]);s.localPlaylistChoice='__new__';s.localNewPlaylistTitle='OST';
    s.uploadLocalFile=async()=>{attempts++;if(attempts===2)throw Error('offline');return {};};
    await s.uploadLocalFiles();assert.equal(s.localFiles.length,1);assert.equal(s.localPlaylistLocked,true);
    assert.equal(s.localTargetPlaylistId,44);await s.uploadLocalFiles();
    assert.equal(creates,1);assert.equal(s.localFiles.length,0);assert.equal(s.localFilesUploading,false);
});
test('playlist creation failure leaves the batch editable and does not signal upload completion',async()=>{
    const calls=[];const {store:s}=downloads(async(url)=>{calls.push(url);return {ok:false,json:async()=>({error:'failed'})};});
    s.setLocalFiles([{name:'a.mp3',size:1}]);s.localPlaylistChoice='__new__';s.localNewPlaylistTitle='OST';
    await s.uploadLocalFiles();assert.equal(s.localPlaylistLocked,false);assert.equal(s.localFilesUploading,false);
    assert.deepEqual(calls,['/api/player/playlists']);assert.equal(s.localFiles.length,1);
});
test('torrent destination locks after start',()=>{
    const {store:s}=downloads();s.currentJob={status:'preview'};assert.equal(s.torrentDestinationLocked(),false);
    s.currentJob.status='paused';assert.equal(s.torrentDestinationLocked(),true);
});

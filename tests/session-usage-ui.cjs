const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const {test}=require('node:test');
const source=fs.readFileSync(path.join(__dirname,'../ui/session-usage.js'),'utf8');
const tick=()=>new Promise(resolve=>setImmediate(resolve));
function fixture(provider='codex') {
  const owner={id:'synthetic-session',provider,accountId:null,directory:'synthetic-project',conversationId:'synthetic-conversation',runId:null};
  const db={preferences:{},active:[],sessions:[owner]},views=new Map(),calls=[],timers=new Map();
  let next=0,read=async()=>response();
  function response(extra={}){return {sessionId:owner.id,provider:owner.provider,accountId:owner.accountId,runId:owner.runId,conversationId:owner.conversationId,available:true,context:{usedTokens:40,limitTokens:100,usedPercent:40},tokens:{totalTokens:100},...extra};}
  const context=vm.createContext({db,views,focusId:owner.id,shownChatId:null,uiLanguage:'en',
    session:id=>db.sessions.find(value=>value.id===id),active:id=>db.active.some(value=>value.id===id),
    document:{visibilityState:'visible'},$(){return null;},$$(){return [];},tr:value=>value,
    api:async(domain,action,args)=>{calls.push({domain,action,args});return read(domain,action,args);},
    setTimeout(fn){timers.set(++next,fn);return next;},clearTimeout(id){timers.delete(id);},
  });
  vm.runInContext(source,context);
  return {context,owner,db,views,calls,timers,response,
    warnings:(data,prefs)=>JSON.parse(JSON.stringify(context.usageWarnings(data,prefs))),
    read:()=>context.readSessionUsage(owner.id),poll:()=>context.pollSessionUsage(),reset:()=>context.resetSessionUsage(),
    entry:()=>context.usageEntry(owner.id),cacheSize:()=>vm.runInContext('sessionUsageCache.size',context),
    setRead(fn){read=fn;},save:values=>context.saveSessionUsagePreferences(values)};
}
test('blank defaults disable both warnings; either strictly exceeded measured threshold warns',()=>{
  const f=fixture();
  for(const provider of ['codex','claude','copilot']) {
    const data={provider,context:{usedPercent:40},tokens:{totalTokens:100}};
    assert.deepEqual(f.warnings(data,{}),{tokens:false,context:false});
    assert.deepEqual(f.warnings(data,{usageTokenWarning:100,usageContextWarning:40}),{tokens:false,context:false});
    assert.deepEqual(f.warnings(data,{usageTokenWarning:99,usageContextWarning:50}),{tokens:true,context:false});
    assert.deepEqual(f.warnings(data,{usageTokenWarning:200,usageContextWarning:39.5}),{tokens:false,context:true});
    assert.deepEqual(f.warnings(data,{usageTokenWarning:99,usageContextWarning:39}),{tokens:true,context:true});
  }
});
test('missing capacity, request-only input and invalid counts cannot invent token totals or warnings',()=>{
  const f=fixture(),preferences={usageTokenWarning:1,usageContextWarning:1};
  for(const data of [null,{}, {context:{usedTokens:100000}}, {tokens:{inputTokens:10000,outputTokens:5000}},
    {context:{usedPercent:'95'},tokens:{totalTokens:'500'}},{context:{usedPercent:NaN},tokens:{totalTokens:Infinity}}])
    assert.deepEqual(f.warnings(data,preferences),{tokens:false,context:false});
});
test('all providers read only their current binding and a failure clears old measurements',async()=>{
  for(const provider of ['codex','claude','copilot']) {
    const f=fixture(provider);await f.read();
    assert.equal(f.entry().data.tokens.totalTokens,100);
    assert.deepEqual(f.calls.map(value=>[value.domain,value.action]),[['session_usage','read']]);
    f.setRead(async()=>{throw Error('Synthetic read failure');});await f.read();
    assert.equal(f.entry().data,null);
  }
});
test('delayed reads cannot cross a conversation, account, provider, directory or live run change',async()=>{
  for(const provider of ['codex','claude','copilot']) for(const change of [
    f=>{f.owner.conversationId='different-conversation';},f=>{f.owner.accountId='different-account';},
    f=>{f.owner.provider='different-provider';},f=>{f.owner.directory='different-project';},
    f=>{f.owner.model='different-model';},
    f=>{f.owner.runId='different-run';},f=>{f.db.active=[{id:f.owner.id,runId:'new-live-run'}];}]) {
    const f=fixture(provider),before=f.response();let resolve;
    f.setRead(()=>new Promise(done=>{resolve=done;}));const pending=f.read();change(f);resolve(before);await pending;
    assert.equal(f.entry().data,null);
  }
});
test('reset invalidates in-flight numeric reads and cache retention remains bounded for hundreds of sessions',async()=>{
  const f=fixture();let resolve;const before=f.response();
  f.setRead(()=>new Promise(done=>{resolve=done;}));const pending=f.read();f.reset();resolve(before);await pending;
  assert.equal(f.cacheSize(),0);
  for(let index=0;index<600;index++) {const id='synthetic-'+index;f.db.sessions.push({...f.owner,id});vm.runInContext('usageEntry('+JSON.stringify(id)+')',f.context);}
  assert.equal(f.cacheSize(),16);
});
test('hidden indicators skip I/O unless a warning threshold is enabled; polling has a fixed request bound',async()=>{
  const f=fixture();f.db.preferences.sessionUsageVisible=false;await f.poll();assert.equal(f.calls.length,0);
  f.db.preferences.usageTokenWarning=50;
  for(let index=0;index<600;index++){const id='synthetic-'+index;f.db.sessions.push({...f.owner,id});f.db.active.push({id,runId:'synthetic-run'});f.views.set(id,{});}
  f.setRead(async(_domain,_action,args)=>{const owner=f.db.sessions.find(value=>value.id===args.id);return f.response({sessionId:owner.id});});
  await f.poll();assert.equal(f.calls.length,2);
  f.context.document.visibilityState='hidden';await f.poll();assert.equal(f.calls.length,2);
});
test('overlapping polls never queue extra provider reads',async()=>{
  const f=fixture();let resolve;
  f.setRead(()=>new Promise(done=>{resolve=done;}));const pending=f.poll();await f.poll();assert.equal(f.calls.length,1);
  resolve(f.response());await pending;
});
test('preference saves serialize, preserve null defaults and recover after a failed write',async()=>{
  const f=fixture();let finish;const order=[];
  f.setRead(async(domain,action,args)=>{assert.equal(domain,'app');assert.equal(action,'preferences');order.push(args);if(order.length===1)await new Promise(resolve=>{finish=resolve;});return {...f.db,preferences:{...f.db.preferences,...args}};});
  const first=f.save({sessionUsageVisible:false}),second=f.save({usageTokenWarning:null,usageContextWarning:80});await tick();assert.equal(order.length,1);finish();await Promise.all([first,second]);
  assert.equal(order.length,2);assert.equal(order[1].usageTokenWarning,null);
  f.setRead(async()=>{throw Error('Synthetic save failure');});await assert.rejects(f.save({usageTokenWarning:10}));
  f.setRead(async()=>({...f.db,preferences:{usageTokenWarning:20}}));await f.save({usageTokenWarning:20});
});

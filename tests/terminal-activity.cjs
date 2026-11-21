const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const assert=require('node:assert/strict'),{test}=require('node:test');
const source=fs.readFileSync(path.join(__dirname,'../ui/app.js'),'utf8');
const code=source.slice(source.indexOf('function detectActivity('),source.indexOf('function observeTerminal('));
function fixture(){
 const activity=new Map(),views=new Map(),timers=new Map();let running=true,calls=0,nextTimer=0,read=async()=>({runId:'one',promptKind:null,ready:true});
 const badge=()=>({replaceWith(){}});
 const view={activityText:'',nativeActivity:null,runId:'one',seq:1,state:badge(),activityTimer:null};views.set('session',view);
 const exports=vm.runInNewContext(code+';({detectActivity,readTerminalActivity:typeof readTerminalActivity === "function" ? readTerminalActivity:null,scheduleTerminalActivity:typeof scheduleTerminalActivity === "function" ? scheduleTerminalActivity:null})',{
  activity,views,active:()=>running,session:()=>({projectId:'project'}),projectId:'project',tr:x=>x,
  renderSessionList(){},renderSessionTabs(){},stateBadge:badge,
  setTimeout(fn){timers.set(++nextTimer,fn);return nextTimer;},clearTimeout(id){timers.delete(id);},
  api:async(...args)=>{calls++;return read(...args);},
 });
 return {...exports,view,activity,views,timers,setRead:fn=>{read=fn;},calls:()=>calls,stop:()=>{running=false;}};
}
test('public CLI help and a completed reply do not imply an approval request',()=>{
 const f=fixture();f.view.activityText='/permissions - choose permissions\nA harmless reply about permissions.\nWorked for 1s\nAsk Codex to do anything';
 f.view.nativeActivity={runId:'one',promptKind:null,ready:true};f.detectActivity('session',f.view);
 assert.equal(f.activity.get('session').key,'completed');
});
test('all providers ignore permission words when no live prompt exists',()=>{
 for(const provider of ['codex','claude','copilot']){
  const f=fixture();f.view.activityText=provider+' permissions approve izin';f.view.nativeActivity={runId:'one',promptKind:null,ready:false};
  f.detectActivity('session',f.view);assert.notEqual(f.activity.get('session').key,'permission');
 }
});
test('old completion text cannot override an unavailable or not-ready current screen',()=>{
 for(const current of [null,{runId:'one',promptKind:null,ready:false}]){
  const f=fixture();f.view.activityText='Worked for 1s\nAsk Codex to do anything';f.view.nativeActivity=current;
  f.detectActivity('session',f.view);assert.equal(f.activity.get('session').key,'running');
 }
});
test('real approval and question states remain distinguishable and clear at readiness',()=>{
 const f=fixture();
 for(const kind of ['approval','question']){
  f.view.nativeActivity={runId:'one',promptKind:kind,ready:false};f.detectActivity('session',f.view);
  assert.equal(f.activity.get('session').key,'permission');assert.equal(f.activity.get('session').label,kind==='approval'?'İzin bekliyor':'Yanıt bekliyor');
 }
 f.view.nativeActivity={runId:'one',promptKind:null,ready:true};f.detectActivity('session',f.view);assert.equal(f.activity.get('session').key,'completed');
});
test('activity reads carry the current run and never send terminal input',async()=>{
 const f=fixture();f.setRead(async(domain,action,args)=>{assert.equal(domain,'terminal');assert.equal(action,'activity');assert.equal(args.runId,'one');return {runId:'one',promptKind:'approval',ready:false};});
 await f.readTerminalActivity('session',f.view);assert.equal(f.activity.get('session').key,'permission');assert.equal(f.calls(),1);
});
test('a reply for a replaced run cannot overwrite its new status',async()=>{
 const f=fixture();let resolve;f.setRead(()=>new Promise(r=>{resolve=r;}));const pending=f.readTerminalActivity('session',f.view);
 f.view.runId='two';resolve({runId:'one',promptKind:'approval',ready:false});await pending;assert.equal(f.activity.size,0);
});
test('output that arrived during a read prevents an older screen from winning',async()=>{
 const f=fixture();let resolve;f.setRead(()=>new Promise(r=>{resolve=r;}));const pending=f.readTerminalActivity('session',f.view);
 f.view.seq++;resolve({runId:'one',promptKind:'approval',ready:false});await pending;assert.equal(f.activity.size,0);
});
test('stopped and detached views ignore in-flight results',async()=>{
 for(const detach of [false,true]){
  const f=fixture();let resolve;f.setRead(()=>new Promise(r=>{resolve=r;}));const pending=f.readTerminalActivity('session',f.view);
  if(detach)f.views.delete('session');else f.stop();resolve({runId:'one',promptKind:'approval',ready:false});await pending;assert.equal(f.activity.size,0);
 }
});
test('burst reads are coalesced and schedule a later current-screen sample',async()=>{
 const f=fixture();let resolve;f.setRead(()=>new Promise(r=>{resolve=r;}));
 const first=f.readTerminalActivity('session',f.view);await f.readTerminalActivity('session',f.view);assert.equal(f.calls(),1);
 resolve({runId:'one',promptKind:null,ready:true});await first;assert.equal(f.timers.size,1);
});

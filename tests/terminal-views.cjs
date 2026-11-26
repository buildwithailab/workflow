const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const assert=require('node:assert/strict'),{test}=require('node:test');
const source=fs.readFileSync(path.join(__dirname,'../ui/app.js'),'utf8');
const code=source.slice(source.indexOf('const MAX_TERMINAL_VIEWS ='),source.indexOf('function disposeTerminalView('));
const {terminalMountPlan:plan,MAX_TERMINAL_VIEWS:limit}=vm.runInNewContext(code+';({terminalMountPlan,MAX_TERMINAL_VIEWS})');
const sessions=Array.from({length:600},(_,index)=>({id:'synthetic-'+index}));
test('hundreds of session cards retain a strict renderer bound at every zoom',()=>{
  for(const z of [0.2,0.75,1.5]) {
    const selected=sessions[550].id;
    const result=plan(sessions,selected,'map',{x:0,y:0,z},{width:1800,height:1000},new Map());
    assert(result.size<=limit); assert(result.has(selected));
  }
});
test('focus and Chat mount only the selected session and never a provider process',()=>{
  for(const [mode,chat] of [['focus',false],['map',true],['grid',true]]) {
    const result=plan(sessions,sessions[400].id,mode,{x:0,y:0,z:1},{width:1800,height:1000},new Map(),chat);
    assert.deepEqual([...result],[sessions[400].id]);
  }
});
test('scrolling a grid changes mounted views while the selected card remains available',()=>{
  const boxes=new Map(sessions.map((s,index)=>[s.id,{x:0,y:(index-300)*520,width:800,height:500}]));
  const result=plan(sessions,sessions[0].id,'grid',{}, {width:1000,height:900},boxes);
  assert(result.size<=limit); assert(result.has(sessions[0].id)); assert(result.has(sessions[300].id));
  assert(!result.has(sessions[100].id));
  assert.equal(plan([],null,'map',{}, {},new Map()).size,0);
});

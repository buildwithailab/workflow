"use strict";
// Native WKWebView/PTY acceptance. All profiles, histories and commands are generated.
const fs=require("node:fs"),path=require("node:path"),os=require("node:os");
const cp=require("node:child_process"),net=require("node:net"),assert=require("node:assert/strict"),crypto=require("node:crypto");
assert.equal(process.platform,"darwin");
const root=path.resolve(__dirname,".."),providers=["codex","claude","copilot"];
const exe=path.resolve(process.env.WORKFLOW_MAC_TEST_EXE||path.join(root,"target/debug/workflow"));
const fixture=path.join(root,"target/test-tools/provider-fixture"),fixtures=[];
let stage="prepare",timeoutStage="",checks=0;
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
async function until(read,label,timeout=30000){const end=Date.now()+timeout;while(Date.now()<end){try{const value=await read();if(value)return value;}catch{}await sleep(60);}timeoutStage=label;throw Error(label);}
function alive(pid){try{process.kill(pid,0);return true;}catch{return false;}}
function wire(endpoint,payload){return new Promise((resolve,reject)=>{
  const socket=net.connect(Number(endpoint.split(":").pop()),"127.0.0.1",()=>socket.write(JSON.stringify(payload)+"\n"));
  let text="";socket.setTimeout(45000,()=>socket.destroy(Error("native request deadline")));socket.on("error",reject);
  socket.on("data",bytes=>{text+=bytes;if(Buffer.byteLength(text)>2*1024*1024){socket.destroy(Error("native response limit"));return;}
    if(text.includes("\n")){socket.end();try{resolve(JSON.parse(text.trim()));}catch{reject(Error("native response rejected"));}}});
  socket.on("end",()=>{if(!text.length)reject(Error("native connection closed"));else if(!text.includes("\n"))reject(Error("native response incomplete"));});
});}
class Native {
  constructor(label,inlineDiff=false){
    this.inlineDiff=inlineDiff;this.pids=new Set();this.directory=fs.mkdtempSync(path.join(os.tmpdir(),"workflow-mac-permissions-"));
    fs.chmodSync(this.directory,0o700);fixtures.push(this);
    this.data=path.join(this.directory,"data");this.home=path.join(this.directory,"home");
    for(const name of ["data","home","tmp","github","usage"])fs.mkdirSync(path.join(this.directory,name),{mode:0o700});
    this.homes=Object.fromEntries(providers.map(p=>[p,path.join(this.data,"test-"+p+"-home")]));
    for(const folder of Object.values(this.homes))fs.mkdirSync(folder,{mode:0o700});
    fs.mkdirSync(path.join(this.data,"updates"));fs.writeFileSync(path.join(this.data,"updates/state.json"),JSON.stringify({automatic:false}));
    this.env={PATH:"/usr/bin:/bin:/usr/sbin:/sbin",SHELL:"/bin/sh",LANG:"en_US.UTF-8",LC_ALL:"en_US.UTF-8",HOME:this.home,
      TMPDIR:path.join(this.directory,"tmp")+path.sep,GH_CONFIG_DIR:path.join(this.directory,"github"),CODEX_HOME:this.homes.codex,
      CLAUDE_CONFIG_DIR:this.homes.claude,COPILOT_HOME:this.homes.copilot,WORKFLOW_DATA_DIR:this.data,WORKFLOW_TEST_MODE:"1",
      WORKFLOW_TEST_PROVIDER:fixture,WORKFLOW_TEST_RPC_EXE:exe,WORKFLOW_TEST_USAGE_DIR:path.join(this.directory,"usage"),
      WORKFLOW_TEST_INPUT_DIR:path.join(this.data,"test-input")};
    this.label=label;
  }
  async start(){
    stage=this.label+" startup";assert(!this.child);
    fs.rmSync(path.join(this.data,"test-control.json"),{force:true});
    this.child=cp.spawn(exe,[],{env:this.env,stdio:"ignore"});this.child.on("error",()=>{});
    this.control=await until(()=>fs.existsSync(path.join(this.data,"test-control.json"))&&JSON.parse(fs.readFileSync(path.join(this.data,"test-control.json"))),"native startup");
    await until(()=>this.eval("!!window.workflow"),"native renderer");
    await this.eval("(()=>{document.querySelector('#dialog-close')?.click();return true})()");
  }
  async request(action,args={}){const response=await wire(this.control.endpoint,{token:this.control.token,action,...args});if(response.error)throw Error(response.error);return response.result;}
  eval(script){return this.request("evaluate",{script});}
  run(script){return this.eval(`(()=>{${script};return true})()`);}
  async api(domain,action,args={}){
    stage=this.label+" "+domain+"."+action;
    if(this.inlineDiff&&domain==="app"&&action==="add_project")fs.writeFileSync(path.join(args.directory,"approval-inline-diff-fixture"),"");
    // The native control listener handles one evaluate at a time. Start the
    // promise without awaiting it in that listener, then poll its reply so the
    // delivery races exercise concurrent application calls rather than a queue.
    const key=crypto.randomUUID();
    await this.eval(`(()=>{const replies=window.__nativePermissionReplies??=new Map();window.workflow.api(${JSON.stringify(domain)},${JSON.stringify(action)},${JSON.stringify(args)}).then(value=>replies.set(${JSON.stringify(key)},{accepted:true,value}),error=>replies.set(${JSON.stringify(key)},{accepted:false,message:String(error?.message??error)}));return true})()`);
    const result=await until(()=>this.eval(`(()=>{const replies=window.__nativePermissionReplies;const result=replies?.get(${JSON.stringify(key)});if(result)replies.delete(${JSON.stringify(key)});return result??null})()`),"native API "+domain+"."+action,45000);
    if(!result.accepted){const error=Error(result.message);error.nativeAction=domain+"."+action;throw error;}
    if(domain==="terminal"&&["start","attach"].includes(action)&&!result.value.exited){assert(Number.isInteger(result.value.pid)&&result.value.pid>0,"live provider PID required");this.pids.add(result.value.pid);}
    return result.value;
  }
  mcp(id,method,params={},override={}){const lease=JSON.parse(fs.readFileSync(path.join(this.data,"leases",id+".json"),"utf8"));return wire(lease.endpoint,{...lease,method,params,...override}).catch(error=>{if(error.message==="native connection closed")return {closed:true};throw error;});}
  page(){return new Locator(this);}
  async select(id){await this.eval(`(async()=>{await window.workflow.refresh();window.workflow.selectSession(${JSON.stringify(id)},true);return true})()`);}
  async stop(){
    if(!this.child)return;
    const current=this.child;assert.equal(current.exitCode,null);assert.equal(current.signalCode,null);
    assert(Array.isArray((await this.api("app","state")).sessions));assert.equal(await this.request("quit"),true);
    await until(()=>current.exitCode!==null||current.signalCode!==null,"native Quit");assert.equal(current.exitCode,0);assert.equal(current.signalCode,null);this.child=null;
    for(const pid of this.pids)await until(()=>!alive(pid),"owned provider stopped on Quit");
  }
  async teardown(){if(!this.child)return;await this.request("quit").catch(()=>{});const child=this.child;await until(()=>child.exitCode!==null||child.signalCode!==null,"fixture teardown",5000).catch(()=>child.kill("SIGTERM"));}
}
// A small DOM driver executes the shared Playwright-shaped test contracts in WKWebView.
// It uses the real application DOM/handlers and native input; no API or PTY is mocked.
class Locator {
  constructor(h,expression="[document]",label="document"){this.h=h;this.expression=expression;this.label=label;}
  find(selector,condition="true",label=selector){return new Locator(this.h,`(${this.expression}).flatMap(root=>Array.from(root.querySelectorAll(${JSON.stringify(selector)}))).filter(element=>${condition})`,label);}
  locator(selector){return this.find(selector);}
  getByRole(role,{name,exact}={}){assert.equal(role,"button");assert.equal(exact,true);return this.find("button",`(element.getAttribute('aria-label')||element.textContent).replace(/\\s+/g,' ').trim()===${JSON.stringify(name)}`,name);}
  getByText(text,{exact}={}){assert.equal(exact,true);return new Locator(this.h,`(${this.expression}).flatMap(root=>[root,...root.querySelectorAll('*')]).filter(element=>element.nodeType===1&&element.textContent.trim()===${JSON.stringify(text)})`,text);}
  filter({hasText}){assert.equal(typeof hasText,"string");return new Locator(this.h,`(${this.expression}).filter(element=>element.textContent.includes(${JSON.stringify(hasText)}))`,this.label);}
  first(){return new Locator(this.h,`(${this.expression}).slice(0,1)`,this.label);}
  async isVisible(){return this.h.eval(`(${this.expression}).some(element=>!!element.getClientRects().length)`);}
  async isHidden(){return !(await this.isVisible());}
  async isDisabled(){return this.h.eval(`(${this.expression})[0]?.disabled===true`);}
  async ready(){await until(()=>this.h.eval(`(()=>{const found=${this.expression};return found.length===1&&!!found[0].getClientRects().length&&!found[0].disabled})()`),"native control "+this.label);}
  async waitFor({state="visible"}={}){if(state==="hidden")return until(()=>this.isHidden(),"native hidden "+this.label);assert.equal(state,"visible");return this.ready();}
  async click(){await this.ready();return this.h.eval(`(()=>{(${this.expression})[0].click();return true})()`);}
  async fill(value){await this.ready();return this.h.eval(`(()=>{const element=(${this.expression})[0];element.focus();element.value=${JSON.stringify(value)};element.dispatchEvent(new Event('input',{bubbles:true}));element.dispatchEvent(new Event('change',{bubbles:true}));return true})()`);}
  async inputValue(){return this.h.eval(`(${this.expression})[0].value`);}
  async focus(){await this.ready();return this.h.eval(`(()=>{(${this.expression})[0].focus();return true})()`);}
  async press(key){assert.equal(key,"Tab");await this.focus();for(const type of ["keyDown","keyUp"])await this.h.request("input",{method:"Input.dispatchKeyEvent",args:{type,key}});}
  async evaluate(fn,arg){await this.ready();return this.h.eval(`(${fn.toString()})((${this.expression})[0],${JSON.stringify(arg)??"undefined"})`);}
}
// Page evaluation takes an argument rather than a selected DOM element.
function page(h){const p=h.page();p.evaluate=(fn,arg)=>h.eval(`(${fn.toString()})(${JSON.stringify(arg)??"undefined"})`);return p;}
async function approvals(inlineDiff=false){
  const h=new Native(inlineDiff?"inline diff":"approval",inlineDiff);
  await require("./chat-approvals.cjs")({start:()=>h.start(),stop:()=>h.stop(),api:(...a)=>h.api(...a),until,sleep,page:()=>page(h),temporary:h.directory,data:h.data,inlineDiff,
    rejectPrompt:(pending,pattern)=>assert.rejects(pending,error=>error.nativeAction==="chat.answer_prompt"&&pattern.test(error.message))});
  checks+=3;console.log("PASS native "+(inlineDiff?"inline diff":"command target")+" identity with all three controlled providers");
}
async function delivery(){
  const h=new Native("delivery");
  await require("./verification-delivery.cjs")({start:()=>h.start(),stop:()=>h.stop(),api:(...a)=>h.api(...a),mcp:(...a)=>h.mcp(...a),until,temporary:h.directory,data:h.data});
  checks+=3;
}
function bind(h,s){
  const id=crypto.randomUUID(),timestamp=new Date().toISOString();
  const [parts,row]=s.provider==="codex"?[["sessions","rollout-"+id+".jsonl"],{type:"session_meta",payload:{id,cwd:s.directory,timestamp}}]:
    s.provider==="claude"?[["projects","synthetic",id+".jsonl"],{type:"user",sessionId:id,cwd:s.directory,timestamp,message:{role:"user",content:"Inspect the generated sample."}}]:
    [["session-state",id,"events.jsonl"],{type:"session.start",timestamp,data:{sessionId:id,context:{cwd:s.directory}}}];
  const file=path.join(h.homes[s.provider],...parts);fs.mkdirSync(path.dirname(file),{recursive:true});fs.writeFileSync(file,JSON.stringify(row)+"\n");return id;
}
async function mcpPermissions(){
  const h=new Native("MCP permission");await h.start();
  try {for(const provider of providers){
    const folder=path.join(h.directory,"verify-"+provider);fs.mkdirSync(folder);
    for(const marker of ["chat-terminal-fixture","verification-terminal-fixture","verification-tool-approval-fixture","codex-mcp-permission-fixture"])fs.writeFileSync(path.join(folder,marker),"");
    fs.writeFileSync(path.join(folder,"sample.txt"),"Generated source\n");
    const p=await h.api("app","add_project",{directory:folder}),s=await h.api("app","create_session",{projectId:p.id,provider,workspaceMode:"direct",tools:true});
    const conversationId=bind(h,s);await h.api("history","bind",{id:s.id,conversationId});const run=await h.api("terminal","start",{id:s.id});await h.select(s.id);
    const window=page(h);await window.locator("#session-chat-view").click();await until(async()=>(await h.api("chat","read",{id:s.id})).ticket,"MCP ready");
    const state=()=>h.api("verification","state",{id:s.id}),before=await state();
    const prepared=await h.api("verification","prepare",{id:s.id,revision:before.revision});
    const menu=await until(async()=>{const prompt=(await h.api("chat","interaction",{id:s.id})).terminalPrompt;return prompt?.kind==="approval"&&prompt.menu;},"MCP actionable permission");
    assert(menu.question.includes("verification_request"));assert(menu.question.includes(prepared.operation));
    const card=window.locator("#chat-terminal-prompt");await card.waitFor();assert(await window.locator("#chat-terminal-output").isHidden());
    const draft="Keep the separate generated follow-up.";await window.locator("#chat-input").fill(draft);assert(await window.locator("#chat-send").isDisabled());
    for(const [changed,message] of [[{runId:crypto.randomUUID()},"Oturum çalıştırması değişti"],[{inputRevision:menu.inputRevision+1},"Soru değişti. Güncel seçeneklerden birini seç."],[{key:crypto.randomUUID()},"Soru değişti. Güncel seçeneklerden birini seç."]])await assert.rejects(()=>h.api("chat","answer_prompt",{id:s.id,runId:run.runId,...menu,choice:1,...changed}),error=>error.nativeAction==="chat.answer_prompt"&&error.message===message);
    const choice=provider==="codex"?2:1;if(provider==="codex")assert(menu.choices[choice-1].includes("this session"));
    const button=card.getByRole("button",{name:choice+" · "+menu.choices[choice-1],exact:true});await button.evaluate(element=>{element.click();element.click();});
    const captured=path.join(h.data,"test-input",provider+"-tool-approval.bin");await until(()=>fs.existsSync(captured),"MCP choice captured");
    assert.equal(fs.readFileSync(captured,"utf8"),provider==="codex"?"\x1b[B\r":"\r");await card.waitFor({state:"hidden"});
    await assert.rejects(()=>h.api("chat","answer_prompt",{id:s.id,runId:run.runId,...menu,choice}),error=>error.nativeAction==="chat.answer_prompt"&&error.message==="Soru değişti. Güncel seçeneklerden birini seç.");assert.equal((await state()).operation,prepared.operation);
    const request=await h.mcp(s.id,"verification_request",{operation:prepared.operation});assert.equal(request.result?.phase,"preparing");
    assert((await h.mcp(s.id,"verification_request",{operation:crypto.randomUUID()})).error);
    const items=[{title:"Inspect generated source",check:"Read the generated file and inspect the fixture result."}];
    assert.equal((await h.mcp(s.id,"verification_propose",{operation:prepared.operation,items})).result?.phase,"ready");
    const attached=await h.api("terminal","attach",{id:s.id});assert.equal(attached.pid,run.pid);assert.equal(attached.runId,run.runId);
    assert.equal((await h.api("chat","read",{id:s.id})).conversationId,conversationId);assert.equal(await window.locator("#chat-input").inputValue(),draft);
    await h.api("terminal","stop",{id:s.id,runId:run.runId});checks++;console.log("PASS "+provider+" native Chat MCP permission with collapsed output, guarded single input and preserved operation/process/conversation/draft");
  }}finally{await h.stop();}
}
async function main(){
  assert(fs.existsSync(exe)&&fs.existsSync(fixture));const selected=process.argv[2]||"all";
  assert(["all","approvals","diff","delivery","mcp"].includes(selected));
  for(const [response,message] of [["","native connection closed"],["{\"result\":","native response incomplete"],["{\n","native response rejected"]]){
    const server=net.createServer(socket=>socket.once("data",()=>socket.end(response)));
    await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
    try{await assert.rejects(wire("127.0.0.1:"+server.address().port,{}),error=>error.message===message);}
    finally{await new Promise(resolve=>server.close(resolve));}
  }
  console.log("PASS native test transport distinguishes empty, partial and malformed responses");
  if(selected==="all"||selected==="approvals")await approvals();
  if(selected==="all"||selected==="diff")await approvals(true);
  if(selected==="all"||selected==="mcp")await mcpPermissions();
  if(selected==="all"||selected==="delivery")await delivery();
  console.log("PASS native Chat permissions and verification: "+checks+" provider groups; controlled CLI fixtures only");
}
module.exports={Native,until,sleep,page,bind};
if(require.main===module)main().catch(error=>{const line=/(chat-approvals|verification-delivery)\.cjs:(\d+)/.exec(error.stack||"");console.error("FAIL native Chat permission acceptance at "+(timeoutStage||error.nativeAction||stage)+(line?" "+line[1]+" line "+line[2]:"")+" ("+error.name+"); raw payload omitted");console.error(String(error.message).replace(/\b[0-9a-f]{8}-[0-9a-f-]{27,}\b/gi,"<id>").replace(/\/(?:Users|private|var|tmp)\/\S+/g,"<fixture>").slice(0,180));process.exitCode=1;}).finally(async()=>{for(const h of fixtures)await h.teardown();});

"use strict";
// Native WKWebView acceptance for the shared features added after macOS 0.1.9.
// Every project, account, journal and command is generated in a private fixture.
const fs=require("node:fs"),path=require("node:path"),os=require("node:os"),cp=require("node:child_process");
const net=require("node:net"),assert=require("node:assert/strict"),crypto=require("node:crypto");
assert.equal(process.platform,"darwin");
const root=path.resolve(__dirname,".."),providers=["codex","claude","copilot"];
const exe=path.resolve(process.env.WORKFLOW_MAC_TEST_EXE||"target/debug/bundle/macos/Workflow.app/Contents/MacOS/workflow");
const fixture=path.join(root,"target/test-tools/provider-fixture"),children=[],fixtures=[];
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const quote=s=>"'"+s.replaceAll("'","'\\''")+"'";
const digest=file=>crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
let checks=0,step="prepare";
function passed(label){checks++;console.log("PASS "+label);}
async function until(read,label,timeout=30000){const end=Date.now()+timeout;while(Date.now()<end){try{const v=await read();if(v)return v;}catch{}await sleep(80);}throw Error(label);}
function alive(pid){try{process.kill(pid,0);return true;}catch{return false;}}
class Native {
  constructor(label,extra={}){
    this.directory=fs.mkdtempSync(path.join(os.tmpdir(),"workflow-mac-features-"+label+"-"));
    fs.chmodSync(this.directory,0o700);fixtures.push(this);
    this.data=path.join(this.directory,"data");this.project=path.join(this.directory,"project");
    this.home=path.join(this.directory,"home");this.usage=path.join(this.directory,"usage");
    for(const folder of [this.data,this.project,this.home,this.usage,path.join(this.directory,"tmp")])fs.mkdirSync(folder,{mode:0o700});
    this.homes=Object.fromEntries(providers.map(p=>[p,path.join(this.directory,p)]));
    for(const folder of Object.values(this.homes))fs.mkdirSync(folder,{mode:0o700});
    fs.mkdirSync(path.join(this.data,"updates"));fs.writeFileSync(path.join(this.data,"updates/state.json"),JSON.stringify({automatic:false}));
    fs.writeFileSync(path.join(this.project,"sample.txt"),"Synthetic source\n");
    this.auth={loggedIn:true,authMethod:"claude.ai",email:crypto.randomUUID()+"@localhost",orgId:crypto.randomUUID(),configDirectory:this.usage};
    fs.writeFileSync(path.join(this.usage,"auth-status.json"),JSON.stringify(this.auth));
    this.env={PATH:"/usr/bin:/bin:/usr/sbin:/sbin",SHELL:"/bin/sh",LANG:"en_US.UTF-8",LC_ALL:"en_US.UTF-8",HOME:this.home,
      TMPDIR:path.join(this.directory,"tmp")+path.sep,GH_CONFIG_DIR:path.join(this.directory,"github"),
      CODEX_HOME:this.homes.codex,CLAUDE_CONFIG_DIR:this.homes.claude,COPILOT_HOME:this.homes.copilot,
      WORKFLOW_DATA_DIR:this.data,WORKFLOW_TEST_MODE:"1",WORKFLOW_TEST_PROVIDER:fixture,WORKFLOW_TEST_RPC_EXE:exe,
      WORKFLOW_TEST_USAGE_DIR:this.usage,WORKFLOW_TEST_QUOTA_DIR:this.project,WORKFLOW_TEST_INPUT_DIR:path.join(this.data,"test-input"),...extra};
  }
  async start(){
    fs.rmSync(path.join(this.data,"test-control.json"),{force:true});
    this.child=cp.spawn(exe,[],{env:this.env,stdio:"ignore"});children.push(this.child);
    this.control=await until(()=>fs.existsSync(path.join(this.data,"test-control.json"))&&JSON.parse(fs.readFileSync(path.join(this.data,"test-control.json"))),"native startup");
    await until(()=>this.eval("!!window.workflow"),"native renderer");
    await this.eval("(()=>{localStorage.setItem('workflow-language','en');const b=document.querySelector('#dialog-close');b?.click();return true})()");
  }
  request(action,args={}){return new Promise((resolve,reject)=>{
    const socket=net.connect(Number(this.control.endpoint.split(":").pop()),"127.0.0.1",()=>socket.write(JSON.stringify({token:this.control.token,action,...args})+"\n"));
    let text="";socket.setTimeout(40000,()=>socket.destroy(Error("native request timeout")));socket.on("error",reject);
    socket.on("data",data=>{text+=data;if(text.includes("\n")){socket.end();try{const r=JSON.parse(text.trim());r.error?reject(Error(r.error)):resolve(r.result);}catch(e){reject(e);}}});
    socket.on("end",()=>{if(!text.includes("\n"))reject(Error("native fixture connection closed"));});
  });}
  eval(script){return this.request("evaluate",{script});}
  run(script){return this.eval(`(()=>{${script};return true})()`);}
  api(domain,action,args={}){return this.eval(`window.workflow.api(${JSON.stringify(domain)},${JSON.stringify(action)},${JSON.stringify(args)})`);}
  async rejects(domain,action,args,pattern){const result=await this.eval(`window.workflow.api(${JSON.stringify(domain)},${JSON.stringify(action)},${JSON.stringify(args)}).then(()=>({accepted:true}),error=>({accepted:false,message:String(error?.message??error)}))`);assert.equal(result.accepted,false,"the application operation must reject through the live control channel");if(pattern)assert.match(result.message,pattern);}
  async select(id){await this.eval(`(async()=>{await window.workflow.refresh();window.workflow.selectSession(${JSON.stringify(id)},true);return true})()`);}
  async stop(){if(!this.child)return;const child=this.child;assert.equal(child.exitCode,null,"application must still be alive before the Quit test");assert.equal(child.signalCode,null,"application must not have received a signal before Quit");const state=await this.api("app","state");assert(Array.isArray(state.sessions),"the control channel must answer a real application-state request");assert.equal(child.exitCode,null);assert.equal(child.signalCode,null);assert.equal(await this.request("quit"),true,"Quit must be acknowledged");await until(()=>child.exitCode!==null||child.signalCode!==null,"native quit");assert.equal(child.signalCode,null);assert.equal(child.exitCode,0);this.child=null;}
  async teardown(){const child=this.child;if(child&&child.exitCode===null&&child.signalCode===null){await this.request("quit").catch(()=>{});await until(()=>child.exitCode!==null||child.signalCode!==null,"fixture teardown",5000).catch(()=>child.kill("SIGTERM"));}}
  async projectCard(){return this.api("app","add_project",{directory:this.project});}
  async session(project,provider,extra={}){return this.api("app","create_session",{projectId:project.id,provider,workspaceMode:"direct",tools:false,...extra});}
  mcp(id,method,params){return new Promise((resolve,reject)=>{const lease=JSON.parse(fs.readFileSync(path.join(this.data,"leases",id+".json"),"utf8"));const socket=net.connect(Number(lease.endpoint.split(":").pop()),"127.0.0.1",()=>socket.write(JSON.stringify({...lease,method,params})+"\n"));let text="";socket.setTimeout(15000,()=>socket.destroy(Error("MCP fixture timeout")));socket.on("error",reject);socket.on("data",data=>{text+=data;if(text.includes("\n")){socket.end();try{resolve(JSON.parse(text.trim()));}catch(e){reject(e);}}});socket.on("end",()=>{if(!text.includes("\n"))reject(Error("MCP fixture connection closed"));});});}
}
async function pathQuota(){
  step="Finder PATH quota";const h=new Native("path");
  const bin=path.join(h.home,".local/bin");fs.mkdirSync(bin,{recursive:true});
  const program="workflow-quota-"+crypto.randomBytes(6).toString("hex");
  fs.writeFileSync(path.join(bin,program),"#!/bin/sh\nprintf '%s\\n' '{\"spent\":3,\"budget\":12}'\n",{mode:0o700});
  await h.start();
  for(const provider of providers){await h.api("app","usage_commands_save",{commands:{[provider]:{kind:"shell",command:program}}});const q=await h.api("app","provider_usage",{provider,refresh:true});assert.equal(q.state,"ready",provider+" Finder PATH query");assert.equal(q.windows[0].remainingPercent,75);}
  assert.deepEqual((await h.api("app","state")).active,[]);await h.stop();passed("all three system quota queries resolve private user CLI tools with Finder's minimal PATH");
}
async function catalogs(){
  step="model context";const h=new Native("models");fs.writeFileSync(path.join(h.project,"model-context-rewrite-fixture"),"");fs.writeFileSync(path.join(h.project,"model-dialog-fixture"),"");await h.start();const p=await h.projectCard();
  for(const provider of providers){
    const config=path.join(h.homes[provider],provider==="codex"?"config.toml":provider==="claude"?"settings.json":"config.json");
    const content=provider==="codex"?"# synthetic configuration\n":"{}";fs.writeFileSync(config,content);
    const requests=()=>fs.readFileSync(path.join(h.project,`model-requests-${provider}.txt`),"utf8").trim().split("\n").length;
    const s=await h.session(p,provider),models=await h.api("models","list",{id:s.id});assert(models.length);assert.equal(requests(),1);
    fs.writeFileSync(config,provider==="codex"?'model = "changed-model"\n':JSON.stringify({model:"changed-model"}));await assert.rejects(()=>h.api("models","apply",{id:s.id,model:models[0].id}));
    fs.unlinkSync(config);assert((await h.api("models","list",{id:s.id,refresh:true})).length);assert.equal(requests(),3);
    fs.writeFileSync(path.join(h.project,"model-context-unstable-fixture"),"");await assert.rejects(()=>h.api("models","list",{id:s.id,refresh:true}));assert.equal(requests(),5);fs.unlinkSync(path.join(h.project,"model-context-unstable-fixture"));
    const stable=await h.api("models","list",{id:s.id,refresh:true});await h.api("models","apply",{id:s.id,model:stable[0].id});
    const before=fs.readFileSync(config),count=requests();fs.writeFileSync(path.join(h.project,"model-context-once-fixture"),"");await h.api("models","list",{id:s.id,refresh:true});fs.writeFileSync(config,before);await h.api("models","list",{id:s.id});assert.equal(requests(),count+3);
    passed(provider+" native model catalog: stable content, single first-run retry, unstable settings and A to B to A cache rejection");
  }
  assert.deepEqual((await h.api("app","state")).active,[]);await h.stop();
}
async function customQuota(){
  step="custom quota";const h=new Native("quota");const spent=1+crypto.randomBytes(1)[0],budget=spent*4;
  fs.writeFileSync(path.join(h.project,"quota-values.txt"),spent+"\n"+budget);fs.writeFileSync(path.join(h.project,"quota-native-id.txt"),crypto.randomUUID());fs.writeFileSync(path.join(h.project,"quota-timestamp.txt"),new Date().toISOString());
  await h.start();const p=await h.projectCard(),sessions=[];
  for(const provider of providers)sessions.push(await h.session(p,provider,{workspaceMode:"isolated"}));
  step="custom quota settings window";
  await h.run("document.querySelector('#app-settings').click()");
  await h.run("Array.from(document.querySelectorAll('#dialog button')).find(b=>b.textContent===tr('Kota sorgulama')).click()");
  await until(()=>h.eval("!!document.querySelector('#quota-command-copilot')"),"quota settings rendered");
  assert(await h.eval("['codex','claude','copilot'].every(p=>document.querySelector('#quota-command-'+p).value==='')"));
  await h.run("document.querySelector('#dialog-close').click()");
  const live=await h.api("terminal","start",{id:sessions[1].id});
  for(const provider of providers){
    step="custom quota "+provider+" slash query";
    const commands={[provider]:{kind:"provider",command:"/usage-fixture:spend"}};await h.api("app","usage_commands_save",{commands});
    let q=await h.api("app","provider_usage",{provider,refresh:true});assert.equal(q.state,"ready");assert.equal(q.windows[0].remainingPercent,75);assert(!JSON.stringify(q).includes("Synthetic hook result"));
    const s=sessions.find(s=>s.provider===provider);assert.deepEqual(await h.api("history","list",{id:s.id}),[]);assert.equal((await h.api("history","discover",{id:s.id})).found,false);
    await assert.rejects(()=>h.api("history","bind",{id:s.id,conversationId:fs.readFileSync(path.join(h.project,`quota-last-id-${provider}.txt`),"utf8")}));
    if(provider==="codex")assert.notEqual(fs.readFileSync(path.join(h.project,"quota-cwd-codex.txt"),"utf8"),s.directory);
    fs.writeFileSync(path.join(h.project,"quota-json-fixture"),"");q=await h.api("app","provider_usage",{provider,refresh:true});assert.equal(q.windows[0].remainingPercent,75);fs.unlinkSync(path.join(h.project,"quota-json-fixture"));
    const input=path.join(h.project,`quota-input-${provider}.txt`),before=fs.statSync(input).size;await h.api("app","provider_usage",{provider});assert.equal(fs.statSync(input).size,before);
    step="custom quota "+provider+" live identity";
    if(provider==="claude")fs.writeFileSync(path.join(h.usage,"auth-status.json"),JSON.stringify({...h.auth,email:crypto.randomUUID()+"@localhost"}));
    else fs.writeFileSync(path.join(h.data,`quota-identity-${provider}.txt`),crypto.randomUUID());
    await h.api("app","provider_usage",{provider});assert(fs.statSync(input).size>before);
    if(provider==="claude"){
      fs.writeFileSync(path.join(h.usage,"auth-status.json"),JSON.stringify({...h.auth,authMethod:"api_key",email:null,orgId:null}));await h.api("app","provider_usage",{provider});const count=fs.statSync(input).size;await h.api("app","provider_usage",{provider});assert(fs.statSync(input).size>count);fs.writeFileSync(path.join(h.usage,"auth-status.json"),JSON.stringify(h.auth));
    }
    await h.api("app","usage_commands_save",{commands:{[provider]:{kind:"shell",command:"printf '%s\\n' "+quote(JSON.stringify({spent,budget}))}}});
    step="custom quota "+provider+" system command";
    q=await h.api("app","provider_usage",{provider,refresh:true});assert.equal(q.state,"ready");assert.equal(q.windows[0].remainingPercent,75);
    for(const command of ["printf '%s\\n' 'synthetic unrecognized output'","exit 1","yes synthetic | head -c 70000"]){await h.api("app","usage_commands_save",{commands:{[provider]:{kind:"shell",command}}});q=await h.api("app","provider_usage",{provider,refresh:true});assert.equal(q.state,"unavailable");assert.deepEqual(q.windows,[]);assert(!JSON.stringify(q).includes("synthetic unrecognized output"));}
    fs.writeFileSync(path.join(h.project,"quota-deny-fixture"),"");await h.api("app","usage_commands_save",{commands});assert.equal((await h.api("app","provider_usage",{provider,refresh:true})).state,"unavailable");fs.unlinkSync(path.join(h.project,"quota-deny-fixture"));
    assert.equal((await h.api("terminal","attach",{id:sessions[1].id})).runId,live.runId);
    passed(provider+" native quota: slash and sh commands, bounded failures, cache and identity, denied permission, reserved history and unchanged live terminal");
  }
  assert(!Buffer.from((await h.api("terminal","attach",{id:sessions[1].id})).data,"base64").toString().includes("/usage-fixture:spend"));
  step="custom quota restart and shutdown";
  await h.api("terminal","stop",{id:sessions[1].id,runId:live.runId});await h.api("app","usage_commands_save",{commands:{}});
  assert.notEqual((await h.api("app","provider_usage",{provider:"copilot"})).source,"custom");await h.stop();await h.start();
  for(const s of sessions)assert.deepEqual(await h.api("history","list",{id:s.id}),[]);assert.deepEqual(await h.api("app","usage_commands_read"),{});
  const commands=Object.fromEntries(providers.map(provider=>[provider,{kind:"provider",command:"/usage-fixture:spend"}]));await h.api("app","usage_commands_save",{commands});
  const inputBefore=Object.fromEntries(providers.map(provider=>[provider,fs.statSync(path.join(h.project,`quota-input-${provider}.txt`)).size]));
  fs.writeFileSync(path.join(h.project,"quota-wait-fixture"),"");for(const provider of providers)fs.rmSync(path.join(h.project,`quota-process-${provider}.txt`),{force:true});
  await h.eval(`(()=>{window.nativeQuotaResults=null;Promise.all(${JSON.stringify(providers)}.map(provider=>window.workflow.api('app','provider_usage',{provider,refresh:true}))).then(r=>window.nativeQuotaResults=r);return true})()`);
  await until(()=>providers.every(provider=>fs.existsSync(path.join(h.project,`quota-process-${provider}.txt`))&&fs.statSync(path.join(h.project,`quota-input-${provider}.txt`)).size>inputBefore[provider]),"three pending submitted quota commands");
  const state=await h.api("data_management","status");await h.rejects("data_management","reset_records",{confirmation:"RESET",revision:state.revision},/kota sorgusu/);
  const pids=providers.map(provider=>Number(fs.readFileSync(path.join(h.project,`quota-process-${provider}.txt`),"utf8")));
  assert(pids.every(alive),"all three pending quota children must be alive before quit");
  await h.stop();for(const pid of pids)await until(()=>!alive(pid),"owned quota probe stopped");passed("native quit stops all three independent quota processes and reset is blocked while a query owns data");
}
function journal(h,s,thread){
  const timestamp=new Date().toISOString(),provider=s.provider;
  const file=provider==="codex"?path.join(h.homes.codex,"sessions","rollout-"+thread+".jsonl"):provider==="claude"?path.join(h.homes.claude,"projects/synthetic",thread+".jsonl"):path.join(h.homes.copilot,"session-state",thread,"events.jsonl");
  const rows=provider==="codex"?[
    {type:"session_meta",payload:{id:thread,cwd:s.directory,timestamp}},
    {type:"event_msg",payload:{type:"token_count",info:{model_context_window:100,last_token_usage:{input_tokens:35,output_tokens:5,total_tokens:40},total_token_usage:{input_tokens:170,output_tokens:30,cached_input_tokens:50,total_tokens:200}}}},
  ]:provider==="claude"?[
    {type:"assistant",sessionId:thread,cwd:s.directory,timestamp,isSidechain:false,message:{id:crypto.randomUUID(),role:"assistant",model:"synthetic-model",content:[],usage:{input_tokens:10,cache_read_input_tokens:20,cache_creation_input_tokens:10}}},
    {type:"cost-state",sessionId:thread,modelUsage:{"synthetic-model":{inputTokens:100,cacheReadInputTokens:50,cacheCreationInputTokens:20,outputTokens:30}}},
  ]:[{type:"session.start",timestamp,data:{sessionId:thread,context:{cwd:s.directory}}},{type:"session.usage_checkpoint",data:{accountingSnapshot:{modelMetrics:{"synthetic-model":{usage:{inputTokens:170,outputTokens:30,cacheReadTokens:50}}}}}}];
  fs.mkdirSync(path.dirname(file),{recursive:true});fs.writeFileSync(file,rows.map(r=>JSON.stringify(r)).join("\n")+"\n");return file;
}
async function sessionUsage(){
  step="session usage";const h=new Native("usage");await h.start();const p=await h.projectCard(),sessions=[];
  for(const provider of providers){
    const s=await h.session(p,provider),thread=crypto.randomUUID(),file=journal(h,s,thread);await h.api("history","bind",{id:s.id,conversationId:thread});sessions.push({...s,thread});
    const value=await h.api("session_usage","read",{id:s.id});assert.equal(value.tokens.totalTokens,200);assert.equal(value.context.usedPercent,provider==="codex"?40:null);assert.equal(value.context.limitTokens,provider==="codex"?100:null);
    await h.select(s.id);await h.run("document.querySelector('#session-chat-view').click()");
    await h.api("app","preferences",{usageTokenWarning:199,usageContextWarning:null,sessionUsageVisible:false});await h.eval(`(async()=>{await window.workflow.refresh();await readSessionUsage(${JSON.stringify(s.id)});return true})()`);
    assert(await h.eval("!!document.querySelector('#chat-session-usage .session-usage-meter.usage-warning')"));
    await h.api("app","preferences",{usageTokenWarning:200});await h.eval("window.workflow.refresh()");assert(!await h.eval("!!document.querySelector('#chat-session-usage .session-usage-meter.usage-warning')"));
    await h.api("app","preferences",{usageTokenWarning:null,usageContextWarning:39,sessionUsageVisible:true});await h.eval("window.workflow.refresh()");assert.equal(await h.eval("!!document.querySelector('#chat-session-usage .session-usage-meter.usage-warning')"),provider==="codex");
    const before=digest(file),run=await h.api("terminal","start",{id:s.id});await h.select(s.id);await h.run("document.querySelector('#session-terminal-view').click();document.querySelector('#session-chat-view').click()");
    assert.equal((await h.api("terminal","attach",{id:s.id})).runId,run.runId);assert.equal((await h.api("app","state")).sessions.find(row=>row.id===s.id).conversationId,thread);
    await h.api("terminal","stop",{id:s.id,runId:run.runId});assert.equal(digest(file),before);
    fs.renameSync(file,file+".held");assert.equal((await h.api("session_usage","read",{id:s.id})).available,false);fs.renameSync(file+".held",file);await h.api("data_management","clear_memory");assert.equal((await h.api("session_usage","read",{id:s.id})).tokens.totalTokens,200);
    passed(provider+" measured native session usage, exact thresholds, unknown capacity, conversation continuity and cache clearing");
  }
  await h.run("document.querySelector('#app-settings').click()");assert(!await h.eval("!!document.querySelector('#session-usage-settings')"));
  await h.run("openSessionUsageSettings()");assert(await h.eval("!!document.querySelector('#session-usage-settings')"));
  await h.run("document.querySelector('#usage-token-warning').value='12345';document.querySelector('#usage-context-warning').value='75.5';document.querySelector('#save-usage-thresholds').click()");
  await until(async()=>(await h.api("app","state")).preferences.usageTokenWarning===12345,"native usage settings save");
  for(const invalid of [{usageTokenWarning:-1},{usageTokenWarning:1.5},{usageContextWarning:101},{sessionUsageVisible:"true"}])await assert.rejects(()=>h.api("app","preferences",invalid));
  await h.stop();await h.start();const state=await h.api("app","state");assert.equal(state.preferences.usageContextWarning,75.5);assert.equal(state.preferences.usageTokenWarning,12345);assert.deepEqual(state.active,[]);assert(!fs.existsSync(path.join(h.project,".git")));await h.stop();passed("separate native usage settings persist without restarting any provider or initializing Git");
}
async function verification(){
  step="verification MCP";const h=new Native("verification");await h.start();const items=[{title:"Synthetic build",check:"Run the synthetic build and inspect exit status."},{title:"Synthetic form",check:"Inspect the fixture form."}],sessions=[];
  for(const provider of providers){
    const folder=path.join(h.directory,"verify-"+provider);fs.mkdirSync(folder);for(const marker of ["chat-terminal-fixture","verification-terminal-fixture"])fs.writeFileSync(path.join(folder,marker),"");
    fs.writeFileSync(path.join(folder,"sample.txt"),"before\n");const p=await h.api("app","add_project",{directory:folder});const s=await h.session(p,provider,{workspaceMode:"isolated",tools:true});sessions.push(s);
    const state=()=>h.api("verification","state",{id:s.id}),perform=async(action,args={})=>h.api("verification",action,{id:s.id,revision:(await state()).revision,...args});
    await h.select(s.id);await h.run("document.querySelector('#session-verification-view').click()");assert.equal((await state()).phase,"idle");assert.deepEqual((await h.api("app","state")).active,[]);await assert.rejects(()=>perform("prepare"));
    const run=await h.api("terminal","start",{id:s.id});await until(async()=>!!(await h.api("chat","read",{id:s.id})).ticket,"verification provider ready");
    const first=await perform("prepare");assert.equal(first.phase,"preparing");const request=await h.mcp(s.id,"verification_request",{operation:first.operation});assert.match(request.result.instructions,/Do NOT run checks/);
    if(sessions.length>1){const other=await h.api("terminal","start",{id:sessions[0].id});assert((await h.mcp(sessions[0].id,"verification_request",{operation:first.operation})).error);await h.api("terminal","stop",{id:sessions[0].id,runId:other.runId});}
    assert(!(await h.mcp(s.id,"verification_propose",{operation:first.operation,items})).error);let ready=await state();assert.equal(ready.phase,"ready");await assert.rejects(()=>h.api("verification","edit",{id:s.id,revision:ready.revision-1,items}));
    await h.eval(`(async()=>{await readVerification(${JSON.stringify(s.id)},false);return true})()`);
    await until(()=>h.eval("Array.from(document.querySelectorAll('#verification-actions button')).some(b=>b.textContent===tr('Listeyi düzenle'))"),"native verification edit action");
    await h.run("Array.from(document.querySelectorAll('#verification-actions button')).find(b=>b.textContent===tr('Listeyi düzenle')).click()");
    await h.run("const input=document.querySelector('.verification-edit-item input');input.value='Synthetic edited build';input.dispatchEvent(new Event('input',{bubbles:true}));Array.from(document.querySelectorAll('#verification-actions button')).find(b=>b.textContent===tr('Listeyi kaydet')).click()");
    ready=await until(async()=>{const v=await state();return v.revision>ready.revision&&v.items[0].title==='Synthetic edited build'&&v;},"native edited checklist saved");
    await until(async()=>!!(await h.api("chat","read",{id:s.id})).ticket,"verification idle composer");const execution=await perform("start");
    assert.deepEqual(execution.items,ready.items);
    const results=(await state()).items.map((item,index)=>({id:item.id,status:index?"blocked":"passed",evidence:index?"Synthetic blocked fixture":"Synthetic measured result"}));
    assert((await h.mcp(s.id,"verification_report",{operation:execution.operation,results:[]})).error);assert(!(await h.mcp(s.id,"verification_report",{operation:execution.operation,results})).error);assert((await h.mcp(s.id,"verification_report",{operation:execution.operation,results})).error);
    await h.select(s.id);await h.run("document.querySelector('#session-chat-view').click()");
    const reported=await state();assert.equal(reported.phase,"reported");assert.equal(reported.results.length,results.length);assert.deepEqual(reported.results,results.map(result=>({...result,source:'model-report'})));assert.equal((await h.api("terminal","attach",{id:s.id})).runId,run.runId);
    await until(async()=>!!(await h.api("chat","read",{id:s.id})).ticket,"verification composer after report");const cancelled=await perform("prepare");await perform("cancel");assert((await h.mcp(s.id,"verification_propose",{operation:cancelled.operation,items})).error);
    await until(async()=>!!(await h.api("chat","read",{id:s.id})).ticket,"verification composer after cancel");const stale=await perform("prepare");fs.appendFileSync(path.join(s.directory,"sample.txt"),"changed\n");assert((await h.mcp(s.id,"verification_propose",{operation:stale.operation,items})).error);assert.equal((await h.api("verification","state",{id:s.id,refresh:true})).phase,"stale");
    // Retain a non-empty, honestly produced report for the restart comparison.
    await until(async()=>!!(await h.api("chat","read",{id:s.id})).ticket,"fresh verification composer");const fresh=await perform("prepare");assert(!(await h.mcp(s.id,"verification_propose",{operation:fresh.operation,items})).error);await until(async()=>!!(await h.api("chat","read",{id:s.id})).ticket,"fresh checklist ready");const finalRun=await perform("start");const finalResults=finalRun.items.map(item=>({id:item.id,status:'blocked',evidence:'Synthetic persisted report'}));assert(!(await h.mcp(s.id,"verification_report",{operation:finalRun.operation,results:finalResults})).error);
    await h.api("terminal","stop",{id:s.id,runId:run.runId});assert(!fs.existsSync(path.join(folder,"verification.json")));passed(provider+" native user-initiated verification: guarded MCP, editable list, explicit start, honest reports, replay/cancel/source/run isolation");
  }
  const retained={};for(const s of sessions){retained[s.id]=await h.api("verification","state",{id:s.id});assert.equal(retained[s.id].items.length,2);assert.equal(retained[s.id].results.length,2);}
  await h.stop();await h.start();for(const s of sessions)assert.deepEqual(await h.api("verification","state",{id:s.id}),retained[s.id]);assert.deepEqual((await h.api("app","state")).active,[]);assert(await h.eval("document.querySelector('#verification-panel').hidden"));await h.stop();passed("every private checklist and report field survives native restart with a collapsed panel and no automatic terminals");
}
async function questions(){
  step="native custom question";const h=new Native("questions");for(const marker of ["chat-terminal-fixture","chat-interaction-fixture"])fs.writeFileSync(path.join(h.project,marker),"");await h.start();const p=await h.projectCard();
  for(const provider of providers){
    const s=await h.session(p,provider),thread=crypto.randomUUID();journal(h,s,thread);await h.api("history","bind",{id:s.id,conversationId:thread});const run=await h.api("terminal","start",{id:s.id});await h.select(s.id);await h.run("document.querySelector('#session-chat-view').click()");
    const ticket=await until(async()=>(await h.api("chat","read",{id:s.id})).ticket,"question initial composer");await h.api("chat","send",{id:s.id,text:"Synthetic native question",...ticket});
    await until(async()=>(await h.api("chat","read",{id:s.id})).terminalPrompt?.kind==="approval","question permission");await h.api("terminal","write",{id:s.id,runId:run.runId,data:"1\r"});
    let old;for(const choice of [2,1]){const menu=await until(async()=>{const menu=(await h.api("chat","read",{id:s.id})).terminalPrompt?.menu;return menu&&menu.key!==old&&menu;},"native choice menu");await h.api("chat","answer_prompt",{id:s.id,runId:run.runId,...menu,choice});old=menu.key;}
    const menu=await until(async()=>{const m=(await h.api("chat","read",{id:s.id})).terminalPrompt?.menu;return m&&m.key!==old&&m;},"native custom answer menu");assert.equal(menu.customChoice,3);
    await until(()=>h.eval("!!document.querySelector('#chat-terminal-prompt .chat-custom-input')"),"native custom input");
    const text="Yalnızca sentetik yapılandırmayı güncelle.";await h.eval(`(()=>{const input=document.querySelector('#chat-terminal-prompt .chat-custom-input');input.value=${JSON.stringify(text)};input.dispatchEvent(new Event('input',{bubbles:true}));return true})()`);
    await assert.rejects(()=>h.api("chat","answer_prompt",{id:s.id,runId:run.runId,...menu,text:"invalid\nanswer"}));assert.equal(await h.eval("document.querySelector('#chat-terminal-prompt .chat-custom-input').value"),text);
    await h.run("document.querySelector('#chat-terminal-prompt .chat-custom-send').click()");const captured=path.join(h.data,"test-input",provider+"-custom.txt");await until(()=>fs.existsSync(captured),"native custom response captured");assert.equal(fs.readFileSync(captured,"utf8"),text);
    await assert.rejects(()=>h.api("chat","answer_prompt",{id:s.id,runId:run.runId,...menu,text}));assert.equal((await h.api("terminal","attach",{id:s.id})).runId,run.runId);await h.api("terminal","stop",{id:s.id,runId:run.runId});passed(provider+" native WebKit custom question: editable rejected draft, exact single Unicode answer, stale reply rejection and same terminal");
  }
  await h.stop();
}
async function maintenance(){
  step="native safe maintenance";const h=new Native("maintenance");await h.start();const p=await h.projectCard(),source=path.join(h.project,"sample.txt"),before=digest(source),account=path.join(h.homes.codex,"synthetic-marker");fs.writeFileSync(account,"Synthetic account preserved\n");
  const direct=await h.session(p,"codex"),copy=await h.session(p,"claude",{workspaceMode:"isolated"});const marker=path.join(copy.directory,"sample.txt");
  let preview=await h.api("data_management","cleanup_preview");assert.equal(preview.blocked.length,0);assert.equal(preview.folders.length,1);const real=fs.realpathSync(h.data),alias=h.data;assert(real!==alias,"Mac fixture must exercise the system temp alias");
  await h.stop();h.data=real;h.env.WORKFLOW_DATA_DIR=real;await h.start();assert.equal((await h.api("data_management","cleanup_preview")).blocked.length,0);
  const canonicalCopy=await h.session(p,"copilot",{workspaceMode:"isolated"});await h.stop();h.data=alias;h.env.WORKFLOW_DATA_DIR=alias;await h.start();assert.equal((await h.api("data_management","cleanup_preview")).blocked.length,0);
  await h.api("data_management","clear_memory");assert.equal(digest(source),before);
  let status=await h.api("data_management","status");await assert.rejects(()=>h.api("data_management","reset_records",{confirmation:"wrong",revision:status.revision}));
  await h.api("data_management","reset_records",{confirmation:"RESET",revision:status.revision});assert.equal((await h.api("app","state")).sessions.length,0);assert(fs.existsSync(marker));assert(fs.existsSync(account));await h.api("data_management","restore_records",{confirmation:"RESTORE"});assert.equal((await h.api("app","state")).sessions.length,3);
  status=await h.api("data_management","status");preview=await h.api("data_management","cleanup_preview");const args={confirmation:"RESET",revision:status.revision,deleteWorkspaces:true,fileConfirmation:"DELETE",cleanupFingerprint:preview.fingerprint};fs.appendFileSync(marker,"changed after preview\n");await assert.rejects(()=>h.api("data_management","reset_records",args));assert(fs.existsSync(marker));
  preview=await h.api("data_management","cleanup_preview");await h.api("data_management","reset_records",{...args,cleanupFingerprint:preview.fingerprint});assert(!fs.existsSync(copy.directory));assert(!fs.existsSync(canonicalCopy.directory));assert.equal(digest(source),before);assert(fs.existsSync(account));assert(!fs.existsSync(path.join(h.project,".git")));await assert.rejects(()=>h.api("data_management","restore_records",{confirmation:"RESTORE"}));await h.stop();passed("native temp-path aliases in both restart directions: reset/restore and physical cleanup preserve source, direct project and accounts");
}
async function githubGuards(){
  step="GitHub account cleanup guards";
  for(const mode of ["default","xdg","explicit","future","late"]){
    const h=new Native("gh-"+mode);delete h.env.GH_CONFIG_DIR;delete h.env.XDG_CONFIG_HOME;
    const config=mode==="xdg"?path.join(h.directory,"xdg/gh"):mode==="explicit"?path.join(h.directory,"explicit"):path.join(h.home,".config/gh");
    if(mode==="xdg")h.env.XDG_CONFIG_HOME=path.dirname(config);if(mode==="explicit")h.env.GH_CONFIG_DIR=config;
    await h.start();const p=await h.projectCard(),s=await h.session(p,"copilot",{workspaceMode:"isolated"}),source=digest(path.join(h.project,"sample.txt")),marker=path.join(s.directory,"synthetic-account-marker");fs.writeFileSync(marker,"Protected synthetic account\n");
    const normal=path.join(h.directory,"external-account");fs.mkdirSync(normal);fs.mkdirSync(path.dirname(config),{recursive:true});
    if(mode==="future"){fs.rmdirSync(path.dirname(config));fs.symlinkSync(s.directory,path.dirname(config));assert(!fs.existsSync(config));}
    else fs.symlinkSync(mode==="late"?normal:s.directory,config);
    const before=await h.api("data_management","cleanup_preview");if(mode==="late"){assert.equal(before.blocked.length,0);fs.unlinkSync(config);fs.symlinkSync(s.directory,config);}else assert(before.blocked.length>0);
    const status=await h.api("data_management","status");await assert.rejects(()=>h.api("data_management","reset_records",{confirmation:"RESET",revision:status.revision,deleteWorkspaces:true,fileConfirmation:"DELETE",cleanupFingerprint:before.fingerprint}));assert(fs.existsSync(marker));assert.equal(digest(path.join(h.project,"sample.txt")),source);await h.stop();passed("native GitHub CLI "+mode+" configuration alias protects existing and future account files at preview and final removal");
  }
}
async function deadlines(){
  step="quota deadlines and process groups";const h=new Native("deadlines");
  fs.writeFileSync(path.join(h.project,"quota-values.txt"),"1\n4");fs.writeFileSync(path.join(h.project,"quota-native-id.txt"),crypto.randomUUID());fs.writeFileSync(path.join(h.project,"quota-timestamp.txt"),new Date().toISOString());fs.writeFileSync(path.join(h.project,"quota-wait-fixture"),"");await h.start();
  const independent=cp.spawn("/bin/sleep",["120"],{env:h.env,stdio:"ignore"});children.push(independent);
  for(const kind of ["provider","shell"]){
    const marker=provider=>path.join(h.directory,kind+"-"+provider),descendant=provider=>marker(provider)+"-child";
    const commands=Object.fromEntries(providers.map(provider=>[provider,{kind,command:kind==="provider"?"/usage-fixture:spend":`echo $$ > ${quote(marker(provider))}; /bin/sleep 60 & echo $! > ${quote(descendant(provider))}; wait`}]));
    await h.api("app","usage_commands_save",{commands});const start=Date.now();
    await h.eval(`(()=>{window.nativeDeadlineResult=null;Promise.all(${JSON.stringify(providers)}.map(provider=>window.workflow.api('app','provider_usage',{provider,refresh:true}))).then(r=>window.nativeDeadlineResult=r);return true})()`);
    await until(()=>providers.every(p=>fs.existsSync(kind==="provider"?path.join(h.project,`quota-process-${p}.txt`):descendant(p))),"three deadline probes");
    const pids=providers.flatMap(p=>kind==="provider"?[Number(fs.readFileSync(path.join(h.project,`quota-process-${p}.txt`),"utf8"))]:[Number(fs.readFileSync(marker(p),"utf8")),Number(fs.readFileSync(descendant(p),"utf8"))]);
    assert(pids.every(alive),"deadline child and descendant PIDs must be alive while the query is pending");
    const results=await until(()=>h.eval("window.nativeDeadlineResult"),"bounded quota deadline",40000);assert.equal(results.length,providers.length);assert(results.every(q=>q.state==="unavailable"&&q.windows.length===0&&q.message==="Özel kota sorgusu zaman aşımına uğradı."));assert(Date.now()-start>=24000&&Date.now()-start<40000);for(const pid of pids)await until(()=>!alive(pid),"deadline process group stopped");assert.equal(independent.exitCode,null);assert.equal(independent.signalCode,null);passed("three "+kind+" quota deadlines stop their owned processes and descendants while preserving another process");
  }
  await h.stop();assert.equal(independent.exitCode,null);assert.equal(independent.signalCode,null);independent.kill();
}
async function network(){
  step="Mac proxy inheritance";
  for(const mode of ["direct","explicit"]){
    const proxy="http://127.0.0.1:9/",h=new Native("network-"+mode,mode==="explicit"?{HTTPS_PROXY:proxy,NO_PROXY:"localhost,127.0.0.1"}:{});
    fs.writeFileSync(path.join(h.data,"network-fixture.json"),JSON.stringify({mode:"fail",proxy}));fs.writeFileSync(path.join(h.data,"network-expected-proxy.txt"),mode==="direct"?"DIRECT":proxy);await h.start();const p=await h.projectCard();
    for(const provider of providers){
      const session=await h.session(p,provider);const login=await h.api("provider_login","start",{provider});await until(async()=>Buffer.from((await h.api("provider_login","poll",{runId:login.runId,since:0})).data,"base64").toString().includes("Automatic proxy environment verified"),"Mac login proxy inheritance");await h.api("provider_login","close",{runId:login.runId});assert((await h.api("models","list",{id:session.id})).length);
      const run=await h.api("terminal","start",{id:session.id});await until(async()=>Buffer.from((await h.api("terminal","attach",{id:session.id})).data,"base64").toString().includes("Automatic proxy environment verified"),"Mac terminal proxy inheritance");assert.equal((await h.api("terminal","start",{id:session.id})).runId,run.runId);await h.api("terminal","stop",{id:session.id,runId:run.runId});
    }
    await h.stop();passed("Mac "+mode+" proxy inheritance works for three-provider login, models and terminal while Windows proxy discovery is disabled");
  }
}
async function storage(){
  step="native archive and Git worktree";const h=new Native("storage");
  const repository=path.join(h.directory,"git-project");fs.mkdirSync(repository);fs.writeFileSync(path.join(repository,"sample.txt"),"Synthetic committed source\n");
  const git=(...args)=>cp.execFileSync("/usr/bin/git",["-C",repository,...args],{env:{...h.env,GIT_CONFIG_NOSYSTEM:"1",GIT_CONFIG_GLOBAL:"/dev/null",GIT_AUTHOR_NAME:"Fixture",GIT_COMMITTER_NAME:"Fixture",GIT_AUTHOR_EMAIL:"fixture@localhost",GIT_COMMITTER_EMAIL:"fixture@localhost"},encoding:"utf8",stdio:["ignore","pipe","ignore"]}).trim();
  // This commit belongs only to the generated disposable Git fixture.
  git("init","-b","main");git("add","sample.txt");git("commit","-m","Synthetic initial state");const head=git("rev-parse","HEAD");await h.start();
  const p=await h.projectCard(),g=await h.api("app","add_project",{directory:repository}),copy=await h.session(p,"codex",{workspaceMode:"isolated"}),worktree=await h.session(g,"copilot",{workspaceMode:"isolated"});step="native storage workspace kind";assert.equal(worktree.workspaceKind,"worktree");
  const source=digest(path.join(h.project,"sample.txt")),gitSource=digest(path.join(repository,"sample.txt")),alias=h.data,canonical=fs.realpathSync(h.data);
  const historical=path.join(h.directory,"historical-profile"),other=path.join(h.directory,"unrelated-folder"),managedCopy=fs.realpathSync(copy.directory);fs.symlinkSync(h.data,historical);fs.mkdirSync(other);fs.writeFileSync(path.join(other,"preserved"),"Synthetic unrelated marker\n");
  await h.stop();const dbPath=path.join(h.data,"workspace.json"),saved=JSON.parse(fs.readFileSync(dbPath,"utf8"));saved.sessions.find(s=>s.id===copy.id).directory=path.join(historical,"workspaces",copy.id,"files");fs.writeFileSync(dbPath,JSON.stringify(saved));await h.start();
  step="native storage archive normalization";for(const s of [copy,worktree]){const archived=await h.api("app","session_patch",{id:s.id,archived:true});assert(fs.existsSync(archived.directory));assert(!fs.existsSync(s.directory));if(s.id===copy.id)assert.equal(archived.archivedFromDirectory,managedCopy);}
  await h.stop();h.data=canonical;h.env.WORKFLOW_DATA_DIR=canonical;await h.start();
  step="native storage alias reopen and unarchive";for(const s of [copy,worktree]){const restored=await h.api("app","session_patch",{id:s.id,archived:false});assert(fs.existsSync(restored.directory));}
  step="native storage old alias retarget";fs.unlinkSync(historical);fs.symlinkSync(other,historical);const restored=(await h.api("app","state")).sessions.find(s=>s.id===copy.id);assert.equal(restored.directory,managedCopy);const run=await h.api("terminal","start",{id:copy.id});
  const cwd=cp.execFileSync("/usr/sbin/lsof",["-a","-p",String(run.pid),"-d","cwd","-Fn"],{encoding:"utf8",timeout:5000,stdio:["ignore","pipe","ignore"]}).split("\n").find(line=>line.startsWith("n"))?.slice(1);assert.equal(fs.realpathSync(cwd),managedCopy);await h.api("terminal","stop",{id:copy.id,runId:run.runId});
  // Exercise the other direction with newly recorded canonical paths.
  step="native storage reverse alias";const second=await h.session(p,"claude",{workspaceMode:"isolated"});await h.api("app","session_patch",{id:second.id,archived:true});await h.stop();h.data=alias;h.env.WORKFLOW_DATA_DIR=alias;await h.start();await h.api("app","session_patch",{id:second.id,archived:false});
  const escape=path.join(copy.directory,"escape");fs.symlinkSync(repository,escape);assert((await h.api("data_management","cleanup_preview")).blocked.length>0);fs.unlinkSync(escape);
  step="native storage cleanup";const preview=await h.api("data_management","cleanup_preview");assert.equal(preview.blocked.length,0);assert.equal(preview.folders.length,3);const status=await h.api("data_management","status");
  await h.api("data_management","reset_records",{confirmation:"RESET",revision:status.revision,deleteWorkspaces:true,fileConfirmation:"DELETE",cleanupFingerprint:preview.fingerprint});
  assert(!fs.existsSync(copy.directory));assert(!fs.existsSync(worktree.directory));assert(!fs.existsSync(second.directory));assert(fs.existsSync(path.join(other,"preserved")));assert.equal(digest(path.join(h.project,"sample.txt")),source);assert.equal(digest(path.join(repository,"sample.txt")),gitSource);assert.equal(git("rev-parse","HEAD"),head);assert(git("branch","--list",worktree.branch).includes(worktree.branch));assert(!fs.existsSync(path.join(h.project,".git")));await h.stop();passed("native copy/archive/worktree moves survive both profile aliases; old alias retarget cannot redirect CLI cwd; cleanup preserves source Git head and branch");
}
async function memory(){
  step="native bounded renderer and memory controls";const h=new Native("memory");await h.start();const p=await h.projectCard(),first=await h.session(p,"codex");await h.stop();
  const file=path.join(h.data,"workspace.json"),saved=JSON.parse(fs.readFileSync(file,"utf8"));
  for(let i=0;i<300;i++)saved.sessions.push({...first,id:crypto.randomUUID(),provider:providers[i%3],name:"Synthetic dormant "+i,conversationId:null,runId:null,state:"stopped"});fs.writeFileSync(file,JSON.stringify(saved));await h.start();
  await h.eval(`(()=>{window.workflow.selectProject(${JSON.stringify(p.id)});return true})()`);
  for(const mode of ["map","grid","focus"]){await h.eval(`(()=>{setMode(${JSON.stringify(mode)});return true})()`);await h.select(first.id);const state=await h.eval("({views:views.size,cards:sessionCards.size,active:window.workflow.state().active.length})");assert(state.views<=8);assert.equal(state.active,0);}
  await h.eval(`(()=>{chatView(${JSON.stringify(first.id)}).draft='Synthetic unsent draft';return true})()`);await h.api("data_management","clear_memory");await h.api("data_management","reset_layout");assert.equal(await h.eval(`chatView(${JSON.stringify(first.id)}).draft`),"Synthetic unsent draft");
  const before=await h.api("data_management","status");await h.api("app","session_patch",{id:first.id,name:"Synthetic changed record"});await assert.rejects(()=>h.api("data_management","reset_records",{confirmation:"RESET",revision:before.revision}));const status=await h.api("data_management","status");await h.api("data_management","reset_records",{confirmation:"RESET",revision:status.revision});assert.equal((await h.api("app","state")).sessions.length,0);await h.api("data_management","restore_records",{confirmation:"RESTORE"});assert.equal((await h.api("app","state")).sessions.length,301);assert.deepEqual((await h.api("app","state")).active,[]);await h.stop();passed("301 dormant native session records keep a bounded renderer; memory/layout clear preserves drafts and reset/restore rejects stale records");
}
async function main(){
  assert(fs.existsSync(fixture));const selected=process.argv[2]||"all";
  assert(["all","path","models","quota","usage","verification","questions","maintenance","github","deadlines","network","storage","memory"].includes(selected),"unknown native test group");
  if(selected==="path"||selected==="all")await pathQuota();
  if(selected==="models"||selected==="all")await catalogs();
  if(selected==="quota"||selected==="all")await customQuota();
  if(selected==="usage"||selected==="all")await sessionUsage();
  if(selected==="verification"||selected==="all")await verification();
  if(selected==="questions"||selected==="all")await questions();
  if(selected==="maintenance"||selected==="all")await maintenance();
  if(selected==="github"||selected==="all")await githubGuards();
  if(selected==="deadlines"||selected==="all")await deadlines();
  if(selected==="network"||selected==="all")await network();
  if(selected==="storage"||selected==="all")await storage();
  if(selected==="memory"||selected==="all")await memory();
  console.log("PASS macOS shared feature acceptance: "+checks+" checks; controlled providers only");
}
main().catch(error=>{console.error("FAIL macOS shared feature acceptance at "+step+" ("+error.name+"); raw payload omitted");console.error(String(error.message).split("\n")[0].replace(/\/(?:Users|private|var|tmp)\/\S+/g,"<fixture>").slice(0,180));process.exitCode=1;}).finally(async()=>{
  for(const h of fixtures)await h.teardown();
  for(const child of children)if(child.exitCode===null&&child.signalCode===null)child.kill("SIGTERM");
});

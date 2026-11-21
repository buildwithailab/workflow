"use strict";
// Real WKWebView + PTY integration, controlled CLI fixtures only. No account cache copies.
const fs = require("node:fs"), path = require("node:path"), os = require("node:os"), cp = require("node:child_process");
const net = require("node:net"), http = require("node:http"), assert = require("node:assert/strict"), {randomUUID} = require("node:crypto");
if (process.platform !== "darwin") throw Error("Run this suite on macOS");
const root = path.resolve(__dirname,".."), temporary = fs.mkdtempSync(path.join(os.tmpdir(),"workflow-mac-"));
const profile = path.join(temporary,"data"), project = path.join(temporary,"project");
const executable = process.env.WORKFLOW_MAC_TEST_EXE || path.join(root,"target/debug/bundle/macos/Workflow.app/Contents/MacOS/workflow");
const fixture = path.join(root,"target/test-tools/provider-fixture");
const homes = Object.fromEntries(["codex","claude","copilot"].map(provider=>[provider,path.join(temporary,provider)]));
fs.mkdirSync(project); fs.writeFileSync(path.join(project,"sample.txt"),"synthetic project\n");
fs.writeFileSync(path.join(project,"chat-terminal-fixture"),"");
fs.writeFileSync(path.join(project,"chat-interaction-fixture"),"");
let child, control, server, independent, clipboardBackup, clipboardLast, clipboardModified=false, clipboardRestored=false, checks = 0;
const sleep = ms => new Promise(resolve=>setTimeout(resolve,ms));
async function until(read,label,timeout=15000) { const end=Date.now()+timeout;let last;while(Date.now()<end){try{const value=await read();if(value)return value;}catch(e){last=e;}await sleep(80);}throw Error(label+(last?": "+last.message:"")); }
async function start() {
  control=null;
  fs.rmSync(path.join(profile,"test-control.json"),{force:true});
  // Exercise normal launch arguments; native window restoration is owned by Workflow.
  child = cp.spawn(executable,[],{stdio:"ignore",env:{...process.env,WORKFLOW_DATA_DIR:profile,WORKFLOW_TEST_MODE:"1",
    WORKFLOW_TEST_PROVIDER:fixture,WORKFLOW_TEST_RPC_EXE:executable,WORKFLOW_TEST_INPUT_DIR:path.join(profile,"test-input"),
    CODEX_HOME:homes.codex,CLAUDE_CONFIG_DIR:homes.claude,COPILOT_HOME:homes.copilot}});
  control = await until(()=>fs.existsSync(path.join(profile,"test-control.json"))&&JSON.parse(fs.readFileSync(path.join(profile,"test-control.json"))),"test startup",30000).catch(error=>{child.kill("SIGTERM");throw error;});
  await until(()=>evaluate("!!window.workflow"),"native renderer startup");
}
function request(action,args={}) { return new Promise((resolve,reject)=>{
  const port=Number(control.endpoint.split(":").pop()),socket=net.connect(port,"127.0.0.1",()=>socket.write(JSON.stringify({token:control.token,action,...args})+"\n"));
  let text="";socket.setTimeout(30000,()=>socket.destroy(Error("native test timeout")));socket.on("error",reject);
  socket.on("data",data=>{text+=data;if(text.includes("\n")){socket.end();try{const result=JSON.parse(text.trim());result.error?reject(Error(result.error)):resolve(result.result);}catch(e){reject(e);}}});
  socket.on("end",()=>{if(!text)reject(Error("test connection closed"));});
}); }
const evaluate = script=>request("evaluate",{script});
const api = (domain,action,args={})=>evaluate(`window.workflow.api(${JSON.stringify(domain)},${JSON.stringify(action)},${JSON.stringify(args)})`).catch(e=>{throw Error(domain+"."+action+": "+e.message);});
const passed = label=>{checks++;console.log("PASS "+label);};
let clipboardChild, clipboardPending, clipboardBuffer="";
const maxClipboardWire=64*1024*1024;
function clipboardSnapshot(value) {
  if(!value||!Array.isArray(value.items)||!Number.isSafeInteger(value.changeCount))throw Error("clipboard helper snapshot rejected");
  return value;
}
async function clipboard(action,args={}) {
  const wire=JSON.stringify({action,...args})+"\n";
  if(Buffer.byteLength(wire)>maxClipboardWire)throw Error("clipboard request exceeds restoration budget");
  if(!clipboardChild){
    const helper=cp.spawn(executable,["--clipboard-test-helper"],{env:{...process.env,WORKFLOW_DATA_DIR:profile,WORKFLOW_TEST_MODE:"1"},stdio:["pipe","pipe","ignore"]});
    clipboardChild=helper;
    helper.stdout.setEncoding("utf8");
    helper.stdout.on("data",data=>{
      clipboardBuffer+=data;
      if(Buffer.byteLength(clipboardBuffer)>maxClipboardWire){clipboardPending?.reject(Error("clipboard helper response exceeds budget"));clipboardPending=null;helper.kill();return;}
      const end=clipboardBuffer.indexOf("\n");if(end<0)return;
      const line=clipboardBuffer.slice(0,end);clipboardBuffer=clipboardBuffer.slice(end+1);
      const pending=clipboardPending;clipboardPending=null;if(!pending)return;
      try{
        const response=JSON.parse(line);
        if(response.ok===true){pending.resolve(response.value);return;}
        // A failed write may already have cleared our clipboard. Remember only
        // its revision so cleanup can restore it without overwriting a new copy.
        if(Number.isSafeInteger(response.modifiedChangeCount)){clipboardLast={items:null,changeCount:response.modifiedChangeCount};clipboardModified=true;}
        pending.reject(Error(response.error==="clipboard-revision-changed"?"clipboard changed outside the test":"clipboard helper operation rejected"));
      }catch{pending.reject(Error("clipboard helper response rejected"));}
    });
    const stopped=()=>{if(clipboardChild===helper){clipboardChild=null;clipboardBuffer="";clipboardPending?.reject(Error("clipboard helper stopped"));clipboardPending=null;}};
    helper.on("exit",stopped);helper.on("error",stopped);helper.stdin.on("error",stopped);
  }
  return new Promise((resolve,reject)=>{assert(!clipboardPending);clipboardPending={resolve,reject};clipboardChild.stdin.write(wire);});
}
async function prepareClipboardBackup() {
  if(clipboardBackup)return;
  // Capture immediately before the first mutation, not at suite startup.
  const snapshot=clipboardSnapshot(await clipboard("snapshot"));
  if(snapshot.items.length>1)throw Error("clipboard test needs a single original item so owner-exit restoration can be verified");
  const restore=JSON.stringify({action:"restore",items:snapshot.items,expectedChangeCount:Number.MAX_SAFE_INTEGER})+"\n";
  if(Buffer.byteLength(restore)>maxClipboardWire-1024)throw Error("original clipboard exceeds restoration budget; test did not modify it");
  clipboardBackup=snapshot;clipboardLast=snapshot;
}
async function mutateClipboard(action,args) {
  await prepareClipboardBackup();
  const result=await clipboard(action,{...args,expectedChangeCount:clipboardLast.changeCount});
  if(!Number.isSafeInteger(result?.changeCount))throw Error("clipboard helper write revision rejected");
  clipboardModified=true;
  clipboardLast={items:null,changeCount:result.changeCount};
  const snapshot=clipboardSnapshot(await clipboard("snapshot"));
  if(snapshot.changeCount!==result.changeCount)throw Error("clipboard changed outside the test");
  clipboardLast=snapshot;
}
const setClipboard=items=>mutateClipboard("restore",{items});
const item=(type,data)=>({type,data:Buffer.from(data).toString("base64")});
async function key(key,modifiers=0) { await request("input",{method:"Input.dispatchKeyEvent",args:{type:"keyDown",key,modifiers}});await request("input",{method:"Input.dispatchKeyEvent",args:{type:"keyUp",key,modifiers}}); }
async function bind(session) {
  const provider=session.provider,thread=provider==="codex"?randomUUID():session.id;
  const folder=path.join(homes[provider],provider==="codex"?"sessions":provider==="claude"?"projects":"session-state",session.id);
  fs.mkdirSync(folder,{recursive:true});
  const row=provider==="codex"?{type:"session_meta",payload:{id:thread,cwd:session.directory,timestamp:session.createdAt}}:
    provider==="claude"?{sessionId:thread,cwd:session.directory,timestamp:session.createdAt,type:"user",message:{content:"Synthetic history"}}:
    {type:"session.start",timestamp:session.createdAt,data:{sessionId:thread,context:{cwd:session.directory}}};
  fs.writeFileSync(path.join(folder,thread+".jsonl"),JSON.stringify(row)+"\n");
  await api("history","bind",{id:session.id,conversationId:thread});return thread;
}
async function quit(action="quit") {
  const current=child;await request(action).catch(()=>{});
  await until(()=>current.exitCode!==null||current.signalCode!==null,"application exit");child=null;
}
async function main() {
  await start();
  assert.equal(fs.statSync(profile).mode&0o077,0,"Mac profile directory must be private");
  assert.equal((await request("native_state")).restorable,false);
  passed("native windows leave workspace restoration to Workflow");
  const p=await api("app","add_project",{directory:project});
  const sessions=[];
  for(const provider of ["codex","claude","copilot"]){
    const session=await api("app","create_session",{projectId:p.id,provider,workspaceMode:"direct",tools:true});
    session.thread=await bind(session);sessions.push(session);
    const models=await api("models","list",{id:session.id,refresh:true});assert(models.length>0);
    const model=models.find(model=>model.efforts?.some(e=>e.reasoningEffort==="medium"))||models[0];
    await api("models","apply",{id:session.id,model:model.id,effort:model.defaultEffort});
    await assert.rejects(()=>api("models","apply",{id:session.id,model:model.id,effort:"invented-effort"}));
    passed(provider+" controlled model catalog and effort validation");
    const before=await api("terminal","attach",{id:session.id});assert.equal(before.exited,true);assert(!before.pid);
    const started=await api("terminal","start",{id:session.id});session.run=started;
    const lease=path.join(profile,"leases",session.id+".json");
    const rpc=cp.spawnSync(executable,["mcp",lease],{input:[{jsonrpc:"2.0",id:1,method:"initialize",params:{}},{jsonrpc:"2.0",id:2,method:"tools/list",params:{}},{jsonrpc:"2.0",id:3,method:"tools/call",params:{name:"session_catalog",arguments:{}}}].map(row=>JSON.stringify(row)).join("\n")+"\n",encoding:"utf8",timeout:10000,stdio:["pipe","pipe","ignore"]});
    assert.equal(rpc.status,0);const responses=rpc.stdout.trim().split("\n").map(line=>JSON.parse(line));
    assert.equal(responses[0].result.serverInfo.name,"workflow");
    assert(responses[1].result.tools.some(tool=>tool.name==="teams_send_message"));
    assert(!responses[2].result.isError);
    passed(provider+" MCP stdio registration and authenticated live-session routing");
    const ticket=await until(async()=>{const state=await api("chat","interaction",{id:session.id});return state.ticket;},provider+" composer ready");
    const snapshot=await api("terminal","attach",{id:session.id});
    await evaluate(`(async()=>{await window.workflow.refresh();window.workflow.selectSession(${JSON.stringify(session.id)});const v=window.workflow.views.get(${JSON.stringify(session.id)});await attach(v,session(${JSON.stringify(session.id)}));document.querySelector('#session-chat-view').click();return true})()`);
    await until(()=>evaluate(`window.workflow.views.get(${JSON.stringify(session.id)})?.nativeActivity?.ready === true`),provider+" live activity ready");
    assert(await evaluate(`(()=>{const v=window.workflow.views.get(${JSON.stringify(session.id)});v.activityText+=${JSON.stringify('\n/permissions - configure permissions\nA reply about approval and izin')};detectActivity(${JSON.stringify(session.id)},v);return v.state.dataset.state==='completed'})()`));
    await assert.rejects(()=>api("terminal","activity",{id:session.id,runId:randomUUID()}));
    const again=await api("terminal","attach",{id:session.id});assert.equal(again.pid,started.pid);assert.equal(again.runId,started.runId);
    assert.equal(fs.existsSync(path.join(profile,"test-input",provider+"-interaction.bin")),false);
    await api("chat","send",{id:session.id,text:"Synthetic first line\nTürkçe second line",...ticket});
    await until(()=>fs.existsSync(path.join(profile,"test-input",provider+"-interaction.bin")),provider+" captured input");
    const bytes=fs.readFileSync(path.join(profile,"test-input",provider+"-interaction.bin"),"utf8");
    assert(bytes.includes("Synthetic first line"));assert(bytes.includes("Türkçe second line"));
    assert.equal((bytes.match(/\x1b\[200~/g)||[]).length,1);assert.equal((bytes.match(/\x1b\[201~/g)||[]).length,1);
    assert(bytes.endsWith("\r"));assert.equal((await api("chat","read",{id:session.id})).conversationId,session.thread);
    await assert.rejects(()=>api("terminal","write",{id:session.id,runId:randomUUID(),data:"stale"}));
    passed(provider+" controlled PTY: attach is read-only, multiline sent once, stale run rejected");
    const approval=await until(async()=>{const r=await api("chat","read",{id:session.id});return r.terminalPrompt?.kind==="approval"&&r;},provider+" approval card");
    assert.equal(approval.ticket,null);
    await until(()=>evaluate(`window.workflow.views.get(${JSON.stringify(session.id)})?.nativeActivity?.promptKind === 'approval' && window.workflow.views.get(${JSON.stringify(session.id)}).state.dataset.state === 'permission'`),provider+" live approval badge");
    await assert.rejects(()=>api("chat","send",{id:session.id,text:"blocked",...ticket}));
    await api("terminal","write",{id:session.id,runId:started.runId,data:"1\r"});
    const question=await until(async()=>{const r=await api("chat","read",{id:session.id});return r.terminalPrompt?.menu;},provider+" question card");
    assert.deepEqual(question.choices,["Only one","All of them"]);
    await until(()=>evaluate(`(()=>{const v=window.workflow.views.get(${JSON.stringify(session.id)});return v?.nativeActivity?.promptKind==='question' && v.state.textContent.trim()===tr('Yanıt bekliyor')})()`),provider+" live question badge");
    passed(provider+" badges ignore historical permission words and follow current approval/question screens");
    await assert.rejects(()=>api("chat","answer_prompt",{id:session.id,...question,runId:randomUUID(),choice:2}));
    await api("chat","answer_prompt",{id:session.id,...question,runId:started.runId,choice:2});
    await until(async()=>{const r=await api("chat","read",{id:session.id});return r.terminalPrompt?.menu?.key!==question.key&&r.terminalPrompt?.menu;},provider+" next question");
    await assert.rejects(()=>api("chat","answer_prompt",{id:session.id,...question,runId:started.runId,choice:1}));
    passed(provider+" permission/question cards block messages and reject stale answers");
  }
  await request("minimize");
  const minimized=await until(async()=>{const state=await request("native_state");return state.minimized&&state;},"native window minimized");
  await until(async()=>{const state=await request("native_state");return state.badge==="!"&&state.popupVisible&&state;},"real Dock attention badge and popup");
  const afterPopup=await request("native_state");assert.equal(afterPopup.keyWindow,false);assert.equal(afterPopup.frontPid,minimized.frontPid);
  const restricted=await request("evaluate",{view:"popup",script:"window.__TAURI__.core.invoke('dispatch',{domain:'app',action:'state',args:{}}).then(()=>false,()=>true)"});
  assert.equal(restricted,true);passed("minimized app shows a real Dock badge and restricted popup without taking foreground focus");
  await api("app","preferences",{notificationsEnabled:false});
  await until(async()=>{const state=await request("native_state");return !state.badge&&!state.popupVisible;},"disabled notifications clear Dock and popup");
  await api("app","preferences",{notificationsEnabled:true});await request("restore");
  passed("disabling notifications clears the actual Dock badge and native popup");
  assert.equal(fs.existsSync(path.join(project,".git")),false);passed("Git-free project stays Git-free");
  const addon=(await api("addons","list")).find(addon=>addon.id==="openspec");
  const installed=await api("addons","install",{id:addon.id,revision:addon.revision});
  assert(installed.find(addon=>addon.id==="openspec").installed);
  const isolated=await api("app","create_session",{projectId:p.id,provider:"codex",workspaceMode:"isolated",tools:false,addons:["openspec"]});
  await bind(isolated);await api("terminal","start",{id:isolated.id});
  await api("terminal","stop",{id:isolated.id});
  assert.notEqual(isolated.directory,project);assert(fs.existsSync(path.join(isolated.directory,".agents/skills/openspec-explore/SKILL.md")));
  assert(!fs.existsSync(path.join(isolated.directory,".git")));assert(!fs.existsSync(path.join(project,".agents")));
  passed("pinned Mac OpenSpec initializes privately and isolated Git-free folders keep project files unchanged");
  server=http.createServer((req,res)=>{res.setHeader("Content-Type","text/html; charset=utf-8");res.end(`<!doctype html><meta charset="utf-8"><title>Native browser fixture</title><style>body{font:18px sans-serif;padding:24px}button,input,select{display:block;margin:16px;width:250px;height:40px}</style><h1>Native browser fixture</h1><button id="increment" onclick="document.querySelector('#count').textContent=String(Number(document.querySelector('#count').textContent)+1);document.querySelector('#trusted').textContent=String(event.isTrusted)">Increment</button><span id="count">0</span><span id="trusted"></span><input aria-label="Name"><input type="checkbox" aria-label="Enabled"><select aria-label="Choice"><option value="a">A</option><option value="b">B</option></select><script>if(location.search.includes('cookie'))document.cookie='workflow_native_fixture=one; path=/'</script>`);});
  await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
  const address="http://127.0.0.1:"+server.address().port;
  const browser=await api("browser","open",{sessionId:sessions[0].id,url:address+"/?cookie",mode:"embedded",newTab:true});
  const read=()=>api("browser","read",{sessionId:sessions[0].id,pageId:browser.id});
  let state=await until(async()=>{const r=await read();return r.text.includes("Native browser fixture")&&r;},"WKWebView load");
  await api("browser","viewport",{pageId:browser.id,x:500,y:160,width:700,height:600,visible:true});
  await sleep(400);
  const screenshot=await api("browser","screenshot",{pageId:browser.id});
  assert(Buffer.from(screenshot.data.split(",")[1],"base64").subarray(1,4).equals(Buffer.from("PNG")));passed("WKWebView screenshot is a real PNG");
  const button=state.elements.find(e=>e.text==="Increment");assert(button);
  await api("browser","action",{pageId:browser.id,action:"click",ref:button.ref,documentId:state.documentId});
  state=await until(async()=>{const r=await read();return /1\s*true/.test(r.text)&&r;},"native trusted click");passed("WKWebView native pointer click has isTrusted=true");
  const field=state.elements.find(e=>e.tag==="input"&&e.type!=="checkbox");
  await api("browser","action",{pageId:browser.id,action:"fill",ref:field.ref,documentId:state.documentId,text:"Synthetic name"});
  assert((await read()).elements.some(e=>e.value==="Synthetic name"));passed("WKWebView structured fill");
  await api("browser","pick",{pageId:browser.id});
  await api("browser","action",{pageId:browser.id,action:"click",x:100,y:120});
  const picked=await until(async()=>{const r=await api("browser","poll",{pageId:browser.id});return r.picked;},"element selection");
  assert(picked.selector);assert(picked.screenshot);passed("WKWebView element selection captures context and image");
  const proof=await api("browser","assert",{pageId:browser.id,check:"text_contains",expected:"Native browser fixture"});assert.equal(proof.passed,true);
  const rejected=await api("browser","assert",{pageId:browser.id,check:"text_contains",expected:"not present",timeoutMs:0});assert.equal(rejected.passed,false);passed("browser assertions distinguish pass and failure");
  const chrome=await api("browser","open",{sessionId:sessions[0].id,url:address+"/?chrome",mode:"chrome",newTab:true});
  const chromeRead=()=>api("browser","read",{pageId:chrome.id});
  const chromeState=await until(async()=>{const state=await chromeRead();return state.text.includes("Native browser fixture")&&state;},"isolated Chrome fixture loaded");
  const chromeButton=chromeState.elements.find(element=>element.text==="Increment");
  await api("browser","action",{pageId:chrome.id,action:"click",ref:chromeButton.ref,documentId:chromeState.documentId});
  await until(async()=>/1\s*true/.test((await chromeRead()).text),"Chrome trusted input");
  const chromeImage=await api("browser","screenshot",{pageId:chrome.id});assert(Buffer.from(chromeImage.data.split(",")[1],"base64").subarray(1,4).equals(Buffer.from("PNG")));
  await api("browser","close",{pageId:chrome.id});
  const chromePort=Number(fs.readFileSync(path.join(profile,"chrome-profiles",sessions[0].id,"DevToolsActivePort"),"utf8").split("\n")[0]);
  const chromeInfo=await (await fetch("http://127.0.0.1:"+chromePort+"/json/version")).json();
  assert(new URL(chromeInfo.webSocketDebuggerUrl).hostname==="127.0.0.1");
  await new Promise((resolve,reject)=>{const socket=new WebSocket(chromeInfo.webSocketDebuggerUrl);let sent=false;const timer=setTimeout(()=>reject(Error("isolated test Chrome close")),5000);socket.onopen=()=>{sent=true;socket.send(JSON.stringify({id:1,method:"Browser.close"}));};socket.onclose=()=>{clearTimeout(timer);sent?resolve():reject(Error("isolated test Chrome connection"));};socket.onerror=()=>{if(!sent){clearTimeout(timer);reject(Error("isolated test Chrome connection"));}};});
  await request("restore");
  passed("optional real Chrome uses an isolated profile, trusted input and PNG screenshots");
  state=await read();const oldDocument=state.documentId;
  const beforeReload=await api("browser","poll",{pageId:browser.id});
  await api("browser","reload",{pageId:browser.id});
  await until(async()=>{const r=await api("browser","poll",{pageId:browser.id});return r.revision>=beforeReload.revision+2;},"reload completes a new document");
  state=await read();
  await assert.rejects(()=>api("browser","action",{pageId:browser.id,action:"click",ref:button.ref,documentId:oldDocument}));
  const freshField=state.elements.find(e=>e.tag==="input"&&e.type!=="checkbox");
  await api("browser","action",{pageId:browser.id,action:"click",ref:freshField.ref,documentId:state.documentId}).catch(()=>{throw Error("fresh native field click");});
  await api("browser","action",{pageId:browser.id,action:"type",text:"Native Türkçe 🦖"}).catch(()=>{throw Error("native text insertion");});
  await until(async()=>(await read()).elements.some(e=>e.value==="Native Türkçe 🦖"),"native input text");
  passed("WKWebView reload rejects stale references and native text input accepts Unicode");
  await api("browser","open",{sessionId:sessions[0].id,pageId:browser.id,url:address+"/?second",newTab:false,mode:"embedded"});
  await until(async()=>(await read()).url.includes("second"),"second navigation");
  await api("browser","back",{pageId:browser.id});await until(async()=>(await read()).url.includes("cookie"),"native back");
  await api("browser","forward",{pageId:browser.id});await until(async()=>(await read()).url.includes("second"),"native forward");
  passed("WKWebView back and forward use real navigation history");
  await assert.rejects(()=>api("preview","start",{sessionId:sessions[0].id,settings:{command:"exit 0",url:address}}));
  assert(server.listening);passed("preview refuses an occupied external port without stopping its owner");
  const probe=net.createServer();await new Promise(resolve=>probe.listen(0,"127.0.0.1",resolve));const previewPort=probe.address().port;await new Promise(resolve=>probe.close(resolve));
  fs.writeFileSync(path.join(project,"preview.cjs"),"require('node:http').createServer((q,s)=>s.end('Synthetic owned preview')).listen(Number(process.env.WORKFLOW_PREVIEW_PORT),'127.0.0.1')");
  const shellQuote=value=>"'"+value.replaceAll("'","'\\''")+"'";
  const previewSettings={command:shellQuote(process.execPath)+" preview.cjs",url:"http://127.0.0.1:"+previewPort+"/"};
  await api("preview","start",{sessionId:sessions[0].id,settings:previewSettings});
  let previewState="starting",previewIssue="";
  const preview=await until(async()=>{const run=await api("preview","status",{sessionId:sessions[0].id});previewState=run?.state;
    previewIssue=run?.message?.includes("başka bir süreç")?"ownership":run?.message?.includes("komutu sonlandı")?"command exit":run?.message?.includes("sahipliği")?"listener query":run?.message?.includes("zamanında")?"readiness timeout":run?.message?.includes("süreç")?"process observation":"other";
    return run?.state==="ready"&&run;},"owned preview startup",20000).catch(()=>{throw Error("owned preview startup ("+previewState+", "+previewIssue+")");});
  assert.equal(await (await fetch(previewSettings.url)).text(),"Synthetic owned preview");
  await api("preview","stop",{sessionId:sessions[0].id,runId:preview.runId});
  await until(async()=>{try{await fetch(previewSettings.url);return false;}catch{return true;}},"owned preview stopped");
  passed("preview starts in the project workspace and stops its real listener");
  await api("preview","start",{sessionId:sessions[0].id});
  await until(async()=>(await api("preview","status",{sessionId:sessions[0].id}))?.state==="ready","preview ready before close",20000);
  await api("browser","hide");
  await evaluate(`(()=>{window.workflow.selectSession(${JSON.stringify(sessions[0].id)});document.querySelector('#session-chat-view').click();const input=document.querySelector('#chat-input');input.value='';input.focus();return true})()`);
  const text="Native clipboard Türkçe 🦖\nsecond line";
  await setClipboard([[item("public.utf8-plain-text",text)]]);
  await assert.rejects(()=>clipboard("restore",{items:[],expectedChangeCount:clipboardLast.changeCount-1}),/clipboard changed outside the test/);
  const unchanged=clipboardSnapshot(await clipboard("snapshot"));
  assert(unchanged.changeCount===clipboardLast.changeCount&&JSON.stringify(unchanged.items)===JSON.stringify(clipboardLast.items),"stale clipboard write must preserve the current copy");
  passed("native clipboard rejects stale revisions before clearing any data");
  await key("v",4);
  await until(()=>evaluate(`document.querySelector('#chat-input').value===${JSON.stringify(text)}`),"native Cmd+V paste");
  passed("real macOS clipboard text and Cmd+V paste without sending");
  const file2=path.join(project,"second.txt");fs.writeFileSync(file2,"synthetic second file\n");
  const {pathToFileURL}=require("node:url");
  await setClipboard([path.join(project,"sample.txt"),file2].map(file=>[item("public.file-url",pathToFileURL(file).href)]));
  assert.equal(clipboardLast.items.length,2,"system clipboard file item count");
  assert.equal((await api("clipboard","read_attachment",{id:sessions[0].id})).attachments.length,2,"native file URL decoding");
  await evaluate("(()=>{document.querySelector('#chat-input').focus();return true})()");
  await key("v",4);
  await until(()=>evaluate("document.querySelectorAll('#chat-images .chat-image-chip').length===2"),"Finder file paste");
  passed("real macOS clipboard multiple file URLs become separate attachments");
  await mutateClipboard("tiff",{png:fs.readFileSync(path.join(root,"icons/icon.png")).toString("base64")});
  await key("v",4);
  await until(()=>evaluate("document.querySelectorAll('#chat-images .chat-image-chip').length===3"),"TIFF screenshot paste");
  passed("real macOS TIFF clipboard image is converted to a PNG attachment");
  if(process.env.WORKFLOW_MAC_NATIVE_DROP==="1"){
    fs.writeFileSync(path.join(project,"native-drop.txt"),"synthetic native drag fixture\n");
    await request("prepare_drop");
    await evaluate(`(()=>{window.workflow.selectSession(${JSON.stringify(sessions[0].id)},true);document.querySelector('#view-focus').click();document.querySelector('#session-chat-view').click();return true})()`);
    await api("app","reveal",{sessionId:sessions[0].id,path:"native-drop.txt"});
    console.log("READY physical Finder drag into the Workflow chat composer");
    await until(()=>evaluate("[...document.querySelectorAll('#chat-images .chat-image-chip')].some(chip=>chip.textContent.includes('native-drop.txt'))"),"physical Finder file drop",90000);
    await request("restore");passed("physical Finder file drag becomes a chat attachment without sending");
  }
  const copied="Workflow clipboard copy Türkçe";
  await evaluate(`(async()=>{document.querySelector('#session-terminal-view').click();const v=window.workflow.views.get(${JSON.stringify(sessions[0].id)});v.term.reset();await new Promise(resolve=>v.term.write(${JSON.stringify(copied)},resolve));v.term.select(0,0,${copied.length});v.term.focus();return true})()`);
  if(await clipboard("revision")!==clipboardLast.changeCount)throw Error("clipboard changed outside the test");
  await key("c",4);
  clipboardLast=await until(async()=>{
    const snapshot=clipboardSnapshot(await clipboard("snapshot"));
    const copiedValue=item("public.utf8-plain-text",copied);
    return snapshot.changeCount!==clipboardLast.changeCount&&snapshot.items.length===1&&snapshot.items[0].some(field=>field.type===copiedValue.type&&field.data===copiedValue.data)&&snapshot;
  },"native Cmd+C copy");
  passed("terminal selection Cmd+C copies into the real system clipboard");
  for(const provider of ["codex","claude","copilot"]){
    const captured=path.join(profile,"test-input",provider+"-interaction.bin");fs.rmSync(captured,{force:true});
    const extra=await api("app","create_session",{projectId:p.id,provider,workspaceMode:"direct",tools:false});
    extra.thread=await bind(extra);
    await evaluate("window.workflow.refresh()");
    await evaluate(`(()=>{window.workflow.selectSession(${JSON.stringify(extra.id)},true);document.querySelector('#view-focus').click();document.querySelector('#session-terminal-view').click();window.workflow.views.get(${JSON.stringify(extra.id)}).control.click();return true})()`);
    extra.run=await until(async()=>{const run=await api("terminal","attach",{id:extra.id});return run.runId&&run;},provider+" explicit UI terminal start");sessions.push(extra);
    await until(()=>evaluate(`(()=>{const v=window.workflow.views.get(${JSON.stringify(extra.id)});return v?.ready&&v.runId===${JSON.stringify(extra.run.runId)}&&v.term.modes.bracketedPasteMode})()`),provider+" native terminal paste ready").catch(async()=>{
      const state=await evaluate(`(()=>{const v=window.workflow.views.get(${JSON.stringify(extra.id)});return {exists:!!v,ready:v?.ready,runMatches:v?.runId===${JSON.stringify(extra.run.runId)},bracketed:v?.term.modes.bracketedPasteMode,connected:v?.host.isConnected,hidden:v?.card.hidden}})()`);
      throw Error(provider+" native terminal paste ready "+JSON.stringify(state));
    });
    await evaluate(`(()=>{window.workflow.views.get(${JSON.stringify(extra.id)}).term.focus();return true})()`);
    await setClipboard([[item("public.utf8-plain-text","Native terminal paste\nTürkçe second line")]]);
    await key("v",4);
    await until(()=>evaluate(`!window.workflow.views.get(${JSON.stringify(extra.id)}).hasPendingInput()`),provider+" clipboard input completed");
    assert(!fs.existsSync(captured),"paste alone must not submit");
    await key("Enter");await until(()=>fs.existsSync(captured),provider+" explicit Enter submits paste");
    const data=fs.readFileSync(captured,"utf8");assert(data.includes("Native terminal paste"));assert(data.includes("Türkçe second line"));
    assert.equal((data.match(/\x1b\[200~/g)||[]).length,1);assert.equal((data.match(/\x1b\[201~/g)||[]).length,1);assert(data.endsWith("\r"));
    passed(provider+" real terminal Cmd+V frames multiline Unicode once and waits for explicit Enter");
  }
  independent=cp.spawn("/bin/sleep",["60"],{stdio:"ignore"});
  const pids=sessions.map(s=>s.run.pid);
  await quit("close");
  await until(async()=>{try{await fetch(previewSettings.url);return false;}catch{return true;}},"application exit stops preview listener");
  passed("application close stops the preview process tree");
  for(const pid of pids)await until(()=>{try{process.kill(pid,0);return false;}catch{return true;}},"owned PTY shutdown");
  assert.equal(independent.exitCode,null);passed("main window close stops every owned PTY and preserves an independent process");
  await start();
  for(const s of sessions){const a=await api("terminal","attach",{id:s.id});assert.equal(a.exited,true);assert(!a.pid);assert.equal((await api("chat","read",{id:s.id})).conversationId,s.thread);}
  passed("reopening retains conversation IDs without automatically starting providers");
  await quit();console.log("PASS macOS integration: "+checks+" checks (controlled providers)");
}
main().catch(e=>{console.error("FAIL macOS integration: "+e.message);process.exitCode=1;}).finally(async()=>{
  if(child)await request("quit").catch(()=>{});
  if(independent){independent.kill();}
  if(server)server.close();
  if(clipboardModified&&clipboardBackup&&clipboardLast){
    try{
      const current=await clipboard("revision");
      if(current===clipboardLast.changeCount){
        await clipboard("restore",{items:clipboardBackup.items,expectedChangeCount:current});
        const restored=clipboardSnapshot(await clipboard("snapshot"));
        if(JSON.stringify(restored.items)!==JSON.stringify(clipboardBackup.items))throw Error("clipboard restore mismatch");
        clipboardRestored=true;
      }else{console.error("UNVERIFIED clipboard changed outside the test; current content preserved");process.exitCode=1;}
    }catch{console.error("FAIL original clipboard could not be verified after restoration");process.exitCode=1;}
  }
  if(clipboardChild){
    const helper=clipboardChild;
    helper.stdin.end();
    try{await until(()=>helper.exitCode!==null||helper.signalCode!==null,"clipboard helper exit",10000);}
    catch{helper.kill();console.error("FAIL clipboard helper did not exit cleanly");process.exitCode=1;}
    if(clipboardRestored){
      try{
        const response=JSON.parse(cp.execFileSync(executable,["--clipboard-test-helper"],{input:JSON.stringify({action:"snapshot"})+"\n",encoding:"utf8",timeout:10000,maxBuffer:maxClipboardWire,env:{...process.env,WORKFLOW_DATA_DIR:profile,WORKFLOW_TEST_MODE:"1"},stdio:["pipe","pipe","ignore"]}));
        // Never expose captured clipboard data through assertions or child errors.
        if(response.ok!==true||JSON.stringify(clipboardSnapshot(response.value).items)!==JSON.stringify(clipboardBackup.items))throw Error("clipboard restore mismatch");
        console.log("PASS original clipboard restored (all captured items and types, after helper exit)");
      }catch{
        console.error("FAIL original clipboard restoration could not be verified after helper exit");process.exitCode=1;
      }
    }
  }
  // Keep only synthetic test data in the OS temporary directory for local diagnosis.
});

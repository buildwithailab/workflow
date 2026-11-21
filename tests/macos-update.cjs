"use strict";
// Native installer acceptance on disposable, real application bundles. No user
// profiles, inherited credentials, signing keys, or installed applications.
const fs = require("node:fs"), path = require("node:path"), cp = require("node:child_process"), os = require("node:os");
const assert = require("node:assert/strict"), crypto = require("node:crypto"), net = require("node:net"), http = require("node:http");
assert.equal(process.platform, "darwin");
const root = path.resolve(__dirname, ".."), mode = process.argv[2] || "helper";
const run = process.env.WORKFLOW_MAC_UPDATE_RUN || "local";
assert(/^[a-zA-Z0-9-]+$/.test(run));assert(["helper", "flow", "normal"].includes(mode));
const receiptDirectory=path.join(root,"target/update-checks",run);
fs.mkdirSync(path.dirname(receiptDirectory),{recursive:true});fs.mkdirSync(receiptDirectory,{mode:0o700});
// LaunchServices does not inherit the developer terminal's Documents access.
// Keep native restart fixtures outside protected user document folders.
const directory=fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(),"workflow-update-"+run+"-")));
fs.chmodSync(directory,0o700);fs.writeFileSync(path.join(receiptDirectory,"location.json"),JSON.stringify({directory}));
const base = path.resolve(process.env.WORKFLOW_MAC_UPDATE_BASE_APP || "target/macos-auto-update/base-0.1.8/Workflow.app");
const archive = path.resolve(process.env.WORKFLOW_MAC_UPDATE_ZIP || "target/macos-auto-update/target-0.1.9/Workflow-Update-macOS-arm64.app.zip");
const debug = path.resolve(process.env.WORKFLOW_MAC_UPDATE_TEST_APP || "target/macos-auto-update/Debug-Workflow.app");
const feedFile = process.env.WORKFLOW_MAC_UPDATE_MANIFEST;
const foreignFile = process.env.WORKFLOW_MAC_UPDATE_FOREIGN_FEED;
const fixture = path.join(root, "target/test-tools/provider-fixture");
const hash = bytes => crypto.createHash("sha256").update(bytes).digest("hex");
const zipBytes = fs.readFileSync(archive), children = [], owned = new Set();
let step = "prepare", server, checks = 0;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const passed = label => {checks++;console.log("PASS " + label);};
async function until(read, label, timeout=60000) {
  const end=Date.now()+timeout;
  while(Date.now()<end) {try {const result=await read();if(result)return result;}catch{}await sleep(100);}
  throw Error(label);
}
function native(program, args) {return cp.execFileSync(program,args,{stdio:["ignore","pipe","pipe"],encoding:"utf8"});}
function plist(app) {return JSON.parse(native("/usr/bin/plutil",["-convert","json","-o","-",path.join(app,"Contents/Info.plist")]));}
const executable = app => path.join(app,"Contents/MacOS",plist(app).CFBundleExecutable);
function verify(app) {native("/usr/bin/codesign",["--verify","--deep","--strict","--all-architectures",app]);}
function tree(directory) {
  const result=[];
  function walk(folder) {for(const name of fs.readdirSync(folder).sort()) {const file=path.join(folder,name),s=fs.lstatSync(file);assert(!s.isSymbolicLink());
    if(s.isDirectory())walk(file);else result.push([path.relative(directory,file),s.mode&0o777,hash(fs.readFileSync(file))]);}}
  walk(directory);return result;
}
function alive(pid) {try {process.kill(pid,0);return true;}catch {return false;}}
function appPids(app) {
  const exe=executable(app);
  return native("/bin/ps",["-axo","pid=,comm="]).split("\n").map(line=>line.trim().match(/^(\d+)\s+(.+)$/))
    .filter(m=>m&&m[2]===exe).map(m=>Number(m[1]));
}
function start(program,args,env) {
  const child=cp.spawn(program,args,{env,stdio:"ignore"});children.push(child);
  child.on("error",()=>{child.failed=true;});return child;
}
async function exited(child,code) {
  await until(()=>child.failed||child.exitCode!==null||child.signalCode!==null,"child exit");
  assert(!child.failed);assert.equal(child.signalCode,null);assert.equal(child.exitCode,code);
}
function profile(folder) {
  const data=path.join(folder,"profile");fs.mkdirSync(data,{mode:0o700});
  const homes={};for(const name of ["home","codex","claude","copilot","github","tmp"]) {homes[name]=path.join(folder,name);fs.mkdirSync(homes[name],{mode:0o700});}
  const providers=Object.fromEntries(["codex","claude","copilot"].map(name=>[name,{executable:path.join(folder,"missing-"+name)}]));
  fs.writeFileSync(path.join(data,"workspace.json"),JSON.stringify({version:1,projects:[],sessions:[],links:[],jobs:[],preferences:{},providers}));
  fs.mkdirSync(path.join(data,"updates"),{mode:0o700});fs.writeFileSync(path.join(data,"updates/state.json"),JSON.stringify({automatic:false}));
  // Generated markers stand for data outside the application bundle.
  const protectedFiles=[];
  for(const name of ["synthetic-account-marker","synthetic-session-marker","synthetic-history-marker"]) {
    const file=path.join(data,name);fs.writeFileSync(file,"Synthetic update preservation marker\n",{mode:0o600});protectedFiles.push(file);
  }
  const before=protectedFiles.map(file=>[file,hash(fs.readFileSync(file)),fs.statSync(file).mode&0o777]);
  const env={PATH:"/usr/bin:/bin:/usr/sbin:/sbin",SHELL:"/bin/sh",LANG:"en_US.UTF-8",LC_ALL:"en_US.UTF-8",
    HOME:homes.home,TMPDIR:homes.tmp+path.sep,WORKFLOW_DATA_DIR:data,CODEX_HOME:homes.codex,
    CLAUDE_CONFIG_DIR:homes.claude,COPILOT_HOME:homes.copilot,GH_CONFIG_DIR:homes.github};
  return {data,homes,env,before};
}
function preserved(p) {
  assert.equal(fs.statSync(p.data).mode&0o777,0o700);
  for(const [file,digest,mode] of p.before) {assert.equal(hash(fs.readFileSync(file)),digest);assert.equal(fs.statSync(file).mode&0o777,mode);}
}
function extract(destination) {native("/usr/bin/ditto",["-x","-k",archive,destination]);}
async function observeNormal(app,p,label) {
  const pid=await until(()=>appPids(app)[0],"normal application restart");owned.add(pid);
  assert(!fs.existsSync(path.join(p.data,"test-control.json")));
  const confirmation=path.join(path.dirname(p.data),"native-ui-verified");
  console.log("READY "+label+": verify normal version and saved sessions in the native UI, then Quit");
  await until(()=>fs.existsSync(confirmation),"native UI verification",240000);
  await until(()=>!alive(pid),"normal application quit");owned.delete(pid);preserved(p);
}
async function helpers() {
  assert(/^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/.test(plist(base).CFBundleShortVersionString));verify(base);
  for(const scenario of ["install","tamper","wrong-version","occupied-backup","rollback","parent-wait","restart"]) {
    step="helper "+scenario;
    const folder=path.join(directory,scenario);fs.mkdirSync(folder,{mode:0o700});
    const p=profile(folder),app=path.join(folder,"Workflow.app");fs.cpSync(base,app,{recursive:true});const before=tree(app);
    const previousInode=fs.statSync(app,{bigint:true}).ino;
    const staging=path.join(folder,".workflow-update-"+crypto.randomUUID());fs.mkdirSync(staging,{mode:0o700});extract(staging);
    const candidate=path.join(staging,"Workflow.app");verify(candidate);const newVersion=plist(candidate).CFBundleShortVersionString;
    const expected=tree(candidate),source=path.join(p.data,"updates/candidate.download");fs.writeFileSync(source,zipBytes);
    const parent=start(scenario==="parent-wait"?"/bin/sleep":"/usr/bin/true",scenario==="parent-wait"?["60"]:[],p.env);
    if(scenario!=="parent-wait")await exited(parent,0);
    const plan={parent:parent.pid,destination:app,receipt:path.join(p.data,"updates/install-result"),source,restart:scenario==="restart",profile:p.data,
      profile_environment:["HOME","CODEX_HOME","CLAUDE_CONFIG_DIR","COPILOT_HOME","GH_CONFIG_DIR"].map(name=>[name,p.env[name]]),
      installed_version:plist(base).CFBundleShortVersionString,installed_hash:hash(fs.readFileSync(executable(base))),
      version:scenario==="wrong-version"?"0.2.0":newVersion,executable_hash:hash(fs.readFileSync(executable(candidate)))};
    const installer=path.join(staging,"installer");fs.copyFileSync(executable(base),installer);fs.chmodSync(installer,0o700);
    fs.writeFileSync(path.join(staging,"install.json"),JSON.stringify(plan));
    if(scenario==="tamper")fs.writeFileSync(path.join(candidate,"Contents/Resources/LICENSE.txt"),"Synthetic resource tamper");
    if(scenario==="occupied-backup")fs.mkdirSync(path.join(staging,"previous.app"));
    if(scenario==="rollback")native("/usr/bin/chflags",["uchg",candidate]);
    const child=start(installer,["--apply-update"],p.env);
    if(scenario==="parent-wait") {await sleep(600);assert.deepEqual(tree(app),before);parent.kill("SIGTERM");await until(()=>parent.signalCode!==null,"synthetic parent exit");}
    const success=["install","parent-wait","restart"].includes(scenario);
    await exited(child,success?0:1);
    if(scenario==="rollback")native("/usr/bin/chflags",["nouchg",candidate]);
    if(success) {assert.deepEqual(tree(app),expected);assert.equal(plist(app).CFBundleShortVersionString,newVersion);assert.notEqual(fs.statSync(app,{bigint:true}).ino,previousInode);}
    else {assert.deepEqual(tree(app),before);assert.equal(fs.statSync(app,{bigint:true}).ino,previousInode);}
    assert(!fs.existsSync(source));preserved(p);
    if(scenario==="restart")await observeNormal(app,p,"helper restart");
    else assert.equal(fs.readFileSync(plan.receipt,"utf8"),success?"installed":"failed");
    if(success)assert(!fs.existsSync(staging));
    passed("normal packaged helper "+scenario+": expected result; data retained");
  }
}
async function normalSmoke() {
  step="final normal package";
  verify(base);
  const bytes=fs.readFileSync(executable(base));
  for(const marker of ["test-control.json","test-startup-exit.json","WORKFLOW_TEST_UPDATE_ORIGIN"])
    assert(!bytes.includes(Buffer.from(marker)),"normal package must exclude test facilities");
  const p=profile(directory),app=path.join(directory,"Workflow.app");fs.cpSync(base,app,{recursive:true});
  const child=start(executable(app),[],p.env);
  await observeNormal(app,p,"final normal package");await exited(child,0);preserved(p);
  passed("final normal package native startup, public channel status, quit and profile preservation");
}
function request(control,action,args={}) {
  return new Promise((resolve,reject)=>{
    const socket=net.connect(Number(control.endpoint.split(":").pop()),"127.0.0.1",()=>socket.write(JSON.stringify({token:control.token,action,...args})+"\n"));
    let text="";socket.setTimeout(30000,()=>socket.destroy(Error("native request timeout")));socket.on("error",reject);
    socket.on("data",data=>{text+=data;if(text.includes("\n")){socket.end();try {const value=JSON.parse(text.trim());value.error?reject(Error("native operation rejected")):resolve(value.result);}catch{reject(Error("invalid native response"));}}});
    socket.on("end",()=>{if(!text)reject(Error("native connection closed"));});
  });
}
const api=(control,domain,action,args={})=>request(control,"evaluate",{script:`window.workflow.api(${JSON.stringify(domain)},${JSON.stringify(action)},${JSON.stringify(args)})`});
async function startDebug(app,p,origin) {
  const env={...p.env,WORKFLOW_TEST_MODE:"1",WORKFLOW_TEST_UPDATE_ORIGIN:origin,WORKFLOW_TEST_PROVIDER:fixture,
    WORKFLOW_TEST_RPC_EXE:executable(app),WORKFLOW_TEST_INPUT_DIR:path.join(p.data,"test-input")};
  const child=start(executable(app),[],env);
  const control=await until(()=>fs.existsSync(path.join(p.data,"test-control.json"))&&JSON.parse(fs.readFileSync(path.join(p.data,"test-control.json"))),"debug native bridge");
  await until(()=>request(control,"evaluate",{script:"!!window.workflow"}),"native renderer");
  return {child,control};
}
async function startProviders(control,p,folder) {
  const project=path.join(folder,"project");fs.mkdirSync(project);fs.writeFileSync(path.join(project,"sample.txt"),"Synthetic update project\n");
  const added=await api(control,"app","add_project",{directory:project});const result=[];
  for(const provider of ["codex","claude","copilot"]) {
    const session=await api(control,"app","create_session",{projectId:added.id,provider,workspaceMode:"direct",tools:true});
    const conversation=crypto.randomUUID(),home=p.homes[provider];
    const history=path.join(home,provider==="codex"?"sessions":provider==="claude"?"projects":"session-state",session.id);fs.mkdirSync(history,{recursive:true});
    const row=provider==="codex"?{type:"session_meta",payload:{id:conversation,cwd:session.directory,timestamp:session.createdAt}}:
      provider==="claude"?{sessionId:conversation,cwd:session.directory,timestamp:session.createdAt,type:"user",message:{content:"Synthetic saved conversation"}}:
      {type:"session.start",timestamp:session.createdAt,data:{sessionId:conversation,context:{cwd:session.directory}}};
    const historyFile=path.join(history,conversation+".jsonl");fs.writeFileSync(historyFile,JSON.stringify(row)+"\n");
    const historyHash=hash(fs.readFileSync(historyFile));await api(control,"history","bind",{id:session.id,conversationId:conversation});
    const catalog=await api(control,"models","list",{id:session.id,refresh:true});assert(catalog.length);
    const model=catalog.find(m=>m.efforts?.some(e=>e.reasoningEffort==="medium"))||catalog[0];
    await api(control,"models","apply",{id:session.id,model:model.id,effort:model.defaultEffort});
    const attached=await api(control,"terminal","attach",{id:session.id});assert(!attached.pid);
    const started=await api(control,"terminal","start",{id:session.id});assert(started.pid);result.push({id:session.id,provider,conversation,pid:started.pid,historyFile,historyHash});
  }
  await request(control,"evaluate",{script:`(async()=>{await window.workflow.refresh();window.workflow.selectProject(${JSON.stringify(added.id)});return true})()`});
  return result;
}
async function flows() {
  assert(feedFile&&foreignFile);verify(debug);
  const manifest=fs.readFileSync(feedFile),foreign=fs.readFileSync(foreignFile);
  const signed=require("../scripts/macos-update-feed.cjs").verifyManifest(manifest,fs.readFileSync(path.join(root,"src/update-public-key.txt"),"utf8"));
  assert.equal(signed.size,zipBytes.length);assert.equal(signed.sha256,hash(zipBytes));
  let responseMode="normal",hits=0;
  server=http.createServer((request,response)=>{
    hits++;
    if(request.url==="/updates/Workflow-Update-macOS-arm64.json") {
      if(responseMode==="unpublished"){response.writeHead(404);response.end();return;}
      let bytes=manifest;
      if(responseMode==="signature") {const value=JSON.parse(manifest);const sig=Buffer.from(value.signature,"base64");sig[0]^=1;value.signature=sig.toString("base64");bytes=Buffer.from(JSON.stringify(value));}
      if(responseMode==="foreign")bytes=foreign;
      response.writeHead(200,{"Content-Type":"application/json","Content-Length":bytes.length});response.end(bytes);
    } else if(request.url===`/releases/download/v${signed.version}-macos/Workflow-Update-macOS-arm64.app.zip`) {
      const bytes=Buffer.from(zipBytes);if(responseMode==="hash")bytes[bytes.length-1]^=1;
      response.writeHead(200,{"Content-Type":"application/zip","Content-Length":bytes.length});response.end(bytes);
    } else {response.writeHead(404);response.end();}
  });
  await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
  const origin=`http://127.0.0.1:${server.address().port}/`;
  const scenarios=["unpublished","signature","foreign","hash","readonly","cancel","exit","restart"];
  const selected=process.env.WORKFLOW_MAC_UPDATE_CASES?.split(",")||scenarios;
  assert(selected.length&&selected.every(name=>scenarios.includes(name)));
  for(const scenario of scenarios.filter(name=>selected.includes(name))) {
    step="flow "+scenario;responseMode=["unpublished","signature","foreign","hash"].includes(scenario)?scenario:"normal";
    const folder=path.join(directory,scenario);fs.mkdirSync(folder,{mode:0o700});const p=profile(folder);
    const apps=path.join(folder,"Applications");fs.mkdirSync(apps,{mode:0o700});const app=path.join(apps,"Workflow.app");fs.cpSync(debug,app,{recursive:true});
    const original=hash(fs.readFileSync(executable(app))),{child,control}=await startDebug(app,p,origin);
    const beforeHits=hits;await sleep(1200);assert.equal(hits,beforeHits,"disabled automatic updates must not request a feed");
    let providers=[];
    if(["exit","restart"].includes(scenario))providers=await startProviders(control,p,folder);
    if(["unpublished","signature","foreign","hash"].includes(scenario)) {
      await assert.rejects(()=>api(control,"updates","download"));const status=await api(control,"updates","status");
      assert.equal(status.errorKind,scenario==="unpublished"?"not_published":"untrusted");assert.equal(status.pendingVersion,null);
      assert.equal(hash(fs.readFileSync(executable(app))),original);
      await request(control,"quit").catch(()=>{});await exited(child,0);preserved(p);
      passed("native update "+scenario+" rejected without replacement");continue;
    }
    if(scenario==="restart") {
      await api(control,"updates","check");assert.equal((await api(control,"updates","status")).pendingVersion,null);
      await api(control,"updates","download");
    } else await api(control,"updates","automatic",{enabled:true});
    await until(async()=>(await api(control,"updates","status")).pendingVersion===signed.version,"verified pending download");
    assert.equal(hash(fs.readFileSync(executable(app))),original,"pending download cannot replace the running app");
    if(scenario==="readonly") {
      fs.chmodSync(apps,0o500);
      try {await assert.rejects(()=>api(control,"updates","install"));assert.equal(child.exitCode,null);assert.equal(hash(fs.readFileSync(executable(app))),original);}
      finally {fs.chmodSync(apps,0o700);}
    }
    if(["cancel","readonly"].includes(scenario)) {
      await api(control,"updates","automatic",{enabled:false});assert.equal((await api(control,"updates","status")).pendingVersion,null);
      assert(!fs.readdirSync(path.join(p.data,"updates")).some(name=>name.endsWith(".download")));
      await request(control,"quit").catch(()=>{});await exited(child,0);assert.equal(hash(fs.readFileSync(executable(app))),original);preserved(p);
      passed("native update "+scenario+": no install and no cached download");continue;
    }
    const saved=JSON.parse(fs.readFileSync(path.join(p.data,"workspace.json")));
    if(scenario==="restart")console.log("READY update restart: click the real Update and restart button in the native UI");
    else await request(control,"quit").catch(()=>{});
    step="flow "+scenario+" old app and provider exit";
    await exited(child,0);for(const provider of providers)await until(()=>!alive(provider.pid),"owned provider must stop");
    fs.rmSync(path.join(p.data,"test-control.json"),{force:true});
    step="flow "+scenario+" replacement and finite helper cleanup";
    await until(()=>hash(fs.readFileSync(executable(app)))!==original,"application replacement");verify(app);
    assert.equal(plist(app).CFBundleShortVersionString,signed.version);
    await until(()=>!fs.readdirSync(path.join(p.data,"updates")).some(name=>name.endsWith(".download"))
      && !fs.readdirSync(apps).some(name=>name.startsWith(".workflow-update-")),"finite installer completion");
    if(scenario==="exit") {
      await sleep(700);assert.equal(appPids(app).length,0,"exit-time update must not restart automatically");
      const env={...p.env};start(executable(app),[],env);
    }
    step="flow "+scenario+" native normal target";
    await observeNormal(app,p,"updated normal application "+scenario);
    step="flow "+scenario+" saved provider settings and conversations";
    const after=JSON.parse(fs.readFileSync(path.join(p.data,"workspace.json")));
    for(const provider of providers) {
      const a=saved.sessions.find(s=>s.id===provider.id),b=after.sessions.find(s=>s.id===provider.id);assert(a&&b);
      // Stopping the old PTY and viewing a saved session legitimately changes
      // runtime state and last-use time. All persisted conversation/settings
      // fields and the provider's history bytes must survive unchanged.
      const stable=value=>Object.fromEntries(Object.entries(value).filter(([key])=>!["state","runId","exitCode","lastUsedAt"].includes(key)));
      const changed=Object.keys({...stable(a),...stable(b)}).filter(key=>JSON.stringify(a[key])!==JSON.stringify(b[key]));
      step="flow "+scenario+" saved fields "+(changed.length?changed.join(","):"preserved");
      assert.deepEqual(stable(b),stable(a));assert.equal(b.conversationId,provider.conversation);
      assert.equal(hash(fs.readFileSync(provider.historyFile)),provider.historyHash);
      assert(!alive(provider.pid));
    }
    assert.equal(JSON.stringify(after.projects),JSON.stringify(saved.projects));preserved(p);
    assert(!fs.readdirSync(apps).some(name=>name.startsWith(".workflow-update-")));
    passed("native signed update "+scenario+": normal target, three providers stopped, saved sessions retained");
  }
}
async function main() {
  if(mode==="normal") {await normalSmoke();console.log("PASS macOS normal package acceptance: 1 case");return;}
  // Only our known package layout is passed to the native extractor.
  native("/usr/bin/python3",["-c",[
    "import sys,zipfile,stat", "z=zipfile.ZipFile(sys.argv[1])", "assert z.testzip() is None",
    "allowed={'Contents/Info.plist','Contents/MacOS/workflow','Contents/Resources/Workflow-Mac.icns','Contents/_CodeSignature/CodeResources','Contents/Resources/LICENSE.txt','Contents/Resources/THIRD-PARTY-NOTICES.txt','Contents/Resources/LICENSE-xterm.txt','Contents/Resources/LICENSE-addon-fit.txt'}",
    "assert len(z.infolist())==12", "assert {i.filename.removeprefix('Workflow.app/') for i in z.infolist() if not i.is_dir()}==allowed",
    "assert all(i.filename.startswith('Workflow.app/') and '..' not in i.filename.split('/') and not stat.S_ISLNK(i.external_attr>>16) for i in z.infolist())",
  ].join("\n"),archive]);
  if(mode==="helper")await helpers();else await flows();
  console.log("PASS macOS update acceptance: "+checks+" cases; synthetic profiles only");
}
main().catch(error=>{console.error("FAIL macOS update "+step+" ("+error.constructor.name+"); diagnostic payload omitted");process.exitCode=1;}).finally(async()=>{
  if(server)await new Promise(resolve=>server.close(resolve));
  for(const child of children)if(child.exitCode===null&&child.signalCode===null)child.kill("SIGTERM");
  for(const pid of owned)if(alive(pid))process.kill(pid,"SIGTERM");
});

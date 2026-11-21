"use strict";
// Assisted native acceptance: dismiss each expected error with OK; quit healthy
// windows normally. Uses only synthetic profiles under ignored target storage.
const fs = require("node:fs"), path = require("node:path"), cp = require("node:child_process");
const assert = require("node:assert/strict"), {createHash} = require("node:crypto");
if (process.platform !== "darwin") throw Error("Run startup acceptance on macOS");
const scenario = process.argv[2], scenarios = ["permissions", "corrupt", "duplicate", "late", "normal"];
assert(scenarios.includes(scenario), "Choose permissions, corrupt, duplicate, late or normal");
const root = path.resolve(__dirname, ".."), run = process.env.WORKFLOW_MAC_STARTUP_RUN || "debug";
assert(/^[a-zA-Z0-9-]+$/.test(run), "Use a simple test run name");
const bridge = process.env.WORKFLOW_MAC_STARTUP_BRIDGE === "1";
assert(scenario !== "late" || bridge, "Late I/O failure requires the debug desktop test build");
const source = process.env.WORKFLOW_MAC_STARTUP_APP || path.join(root, "target/debug/bundle/macos/Workflow.app");
const directory = path.join(root, "target/startup-checks", run, scenario);
fs.mkdirSync(path.dirname(directory), {recursive:true}); fs.mkdirSync(directory);
const bundle = path.join(directory, "Workflow.app"), profile = path.join(directory, "profile");
fs.cpSync(source, bundle, {recursive:true}); fs.mkdirSync(profile, {mode:0o700});
for(const name of ["home","tmp","codex","claude","copilot","github"])
  fs.mkdirSync(path.join(directory,name),{mode:0o700});
const children = [];
const hash = bytes => createHash("sha256").update(bytes).digest("hex");
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(read, message, timeout=120000) {
  const end=Date.now()+timeout;
  while(Date.now()<end) { if(await read()) return; await sleep(80); }
  throw Error(message);
}
function start(app) {
  // Build an allowlist from scratch; never forward host authentication or CLI
  // overrides. Explicit provider paths below also block fixed Homebrew discovery.
  const env={PATH:"/usr/bin:/bin:/usr/sbin:/sbin",SHELL:"/bin/sh",LANG:"en_US.UTF-8",LC_ALL:"en_US.UTF-8",
    HOME:path.join(directory,"home"),TMPDIR:path.join(directory,"tmp")+path.sep,WORKFLOW_DATA_DIR:profile,
    CODEX_HOME:path.join(directory,"codex"),CLAUDE_CONFIG_DIR:path.join(directory,"claude"),
    COPILOT_HOME:path.join(directory,"copilot"),GH_CONFIG_DIR:path.join(directory,"github")};
  if(bridge) env.WORKFLOW_TEST_MODE="1";
  const child=cp.spawn(path.join(app,"Contents/MacOS/workflow"),[],{env,stdio:"ignore"});
  children.push(child); child.on("error",()=>{ child.startFailed=true; }); return child;
}
async function exited(child) {
  await until(()=>child.startFailed||child.exitCode!==null||child.signalCode!==null,"startup acceptance timed out");
  assert(!child.startFailed,"Application must start");
  assert.equal(child.signalCode,null,"No signal or SIGABRT termination");
}
function exitState(completed) {
  if(!bridge) return;
  const state=JSON.parse(fs.readFileSync(path.join(profile,"test-startup-exit.json"),"utf8"));
  assert.deepEqual(state,{startupCompleted:completed,exitInstallationStarted:completed},
    "Only a fully started application may enter exit-time update installation");
}
async function main() {
  let primary;
  const marker=path.join(profile,"synthetic-file"); fs.writeFileSync(marker,"synthetic data preserved\n");
  const database=path.join(profile,"workspace.json");
  const providers=Object.fromEntries(["codex","claude","copilot"].map(name=>[name,
    {executable:path.join(directory,"missing-"+name)}]));
  fs.writeFileSync(database,JSON.stringify({version:1,projects:[],sessions:[],links:[],jobs:[],preferences:{},providers}));
  fs.mkdirSync(path.join(profile,"updates"));
  fs.writeFileSync(path.join(profile,"updates/state.json"),JSON.stringify({automatic:false}));
  const protectedFiles=[marker,database];
  if(scenario==="permissions") {
    fs.writeFileSync(path.join(profile,"instance.lock"),"");
    fs.chmodSync(profile,0o755);
  } else if(scenario==="corrupt") {
    fs.writeFileSync(database,"{synthetic invalid JSON");
  } else if(scenario==="late") {
    fs.mkdirSync(path.join(profile,"test-control.json"));
  } else if(scenario==="duplicate") {
    const primaryBundle=path.join(directory,"Primary.app");fs.cpSync(source,primaryBundle,{recursive:true});
    primary=start(primaryBundle);
    await until(()=>fs.existsSync(path.join(profile,"instance.lock"))||primary.exitCode!==null||primary.signalCode!==null,"primary instance startup");
    assert.equal(primary.exitCode,null);assert.equal(primary.signalCode,null);
  }
  const before=protectedFiles.map(file=>({file,hash:hash(fs.readFileSync(file)),mode:fs.statSync(file).mode&0o777}));
  const profileMode=fs.statSync(profile).mode&0o777;
  const child=start(bundle);
  console.log("READY "+scenario+": "+(scenario==="normal"?"verify the normal window, then Quit":"verify the error dialog, then dismiss OK"));
  await exited(child); assert.equal(child.exitCode,scenario==="normal"?0:1);
  for(const item of before) {
    assert.equal(hash(fs.readFileSync(item.file)),item.hash,"Existing data must be unchanged");
    assert.equal(fs.statSync(item.file).mode&0o777,item.mode,"Existing file modes must be unchanged");
  }
  assert.equal(fs.statSync(profile).mode&0o777,profileMode,"Rejected profile must not be chmodded");
  if(scenario==="late") {
    exitState(false);
    assert(!fs.existsSync(path.join(profile,"ready.json")),"Failed setup must not announce readiness");
    assert(!fs.existsSync(path.join(profile,"updates/install-result")),"Failed setup must not start an installer");
  } else if(scenario==="normal") exitState(true);
  if(primary) {
    assert.equal(primary.exitCode,null,"The first application must stay running");
    assert.equal(primary.signalCode,null);
    console.log("READY primary: verify the first application is usable, then Quit Primary.app");
    await exited(primary);assert.equal(primary.exitCode,0);exitState(true);
  }
  console.log("PASS startup "+scenario+": expected exit, no abort, data and permissions retained"+
    (bridge&&(scenario==="late"||scenario==="normal"||scenario==="duplicate")?", exit-time updater state verified":""));
}
main().catch(()=>{console.error("FAIL startup "+scenario+"; no diagnostic payload printed");process.exitCode=1;}).finally(async()=>{
  for(const child of children) if(child.exitCode===null&&child.signalCode===null) child.kill("SIGTERM");
  await Promise.all(children.map(child=>child.exitCode!==null||child.signalCode!==null?null:new Promise(resolve=>child.once("exit",resolve))));
});

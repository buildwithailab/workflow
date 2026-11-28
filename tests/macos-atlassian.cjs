"use strict";
// Real WKWebView, PTY, MCP and Keychain; only generated profiles and CLI fixtures.
const fs=require("node:fs"),path=require("node:path"),os=require("node:os");
const cp=require("node:child_process"),net=require("node:net"),assert=require("node:assert/strict");
assert.equal(process.platform,"darwin");
const root=path.resolve(__dirname,".."),temporary=fs.mkdtempSync(path.join(os.tmpdir(),"workflow-mac-atlassian-"));
fs.chmodSync(temporary,0o700);
const data=path.join(temporary,"data"),project=path.join(temporary,"project");
const exe=path.resolve(process.env.WORKFLOW_MAC_TEST_EXE||path.join(root,"target/debug/workflow"));
const fixture=path.join(root,"target/test-tools/provider-fixture"),providers=["codex","claude","copilot"];
for(const folder of [data,project,...["tmp","github","usage",...providers].map(n=>path.join(temporary,n))])fs.mkdirSync(folder,{mode:0o700});
fs.mkdirSync(path.join(data,"updates"));fs.writeFileSync(path.join(data,"updates/state.json"),JSON.stringify({automatic:false}));
fs.writeFileSync(path.join(project,"sample.txt"),"Generated project\n");
const environment={PATH:"/usr/bin:/bin:/usr/sbin:/sbin",SHELL:"/bin/sh",LANG:"en_US.UTF-8",LC_ALL:"en_US.UTF-8",
  // Security.framework needs the real macOS HOME to resolve the default vault.
  // Every provider configuration, usage source and app data path stays isolated.
  HOME:os.homedir(),TMPDIR:path.join(temporary,"tmp")+path.sep,GH_CONFIG_DIR:path.join(temporary,"github"),
  CODEX_HOME:path.join(temporary,"codex"),CLAUDE_CONFIG_DIR:path.join(temporary,"claude"),COPILOT_HOME:path.join(temporary,"copilot"),
  WORKFLOW_DATA_DIR:data,WORKFLOW_TEST_MODE:"1",WORKFLOW_TEST_PROVIDER:fixture,WORKFLOW_TEST_RPC_EXE:exe,
  WORKFLOW_TEST_USAGE_DIR:path.join(temporary,"usage"),WORKFLOW_TEST_INPUT_DIR:path.join(data,"test-input")};
let child,control,stage="prepare",timeoutStage="",cleanupVerified=false;
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
async function until(read,label,timeout=30000){const end=Date.now()+timeout;while(Date.now()<end){try{const value=await read();if(value)return value;}catch{}await sleep(80);}timeoutStage=label;throw Error(label);}
function wire(endpoint,payload){return new Promise((resolve,reject)=>{
  const socket=net.connect(Number(endpoint.split(":").pop()),"127.0.0.1",()=>socket.write(JSON.stringify(payload)+"\n"));
  let text="";socket.setTimeout(45000,()=>socket.destroy(Error("native request deadline")));socket.on("error",reject);
  socket.on("data",bytes=>{text+=bytes;if(Buffer.byteLength(text)>2*1024*1024){socket.destroy(Error("bounded native response"));return;}
    if(text.includes("\n")){socket.end();try{resolve(JSON.parse(text.trim()));}catch{reject(Error("native response rejected"));}}});
  socket.on("end",()=>{if(!text.includes("\n"))reject(Error("native connection closed"));});
});}
async function request(action,args={}){const response=await wire(control.endpoint,{token:control.token,action,...args});if(response.error)throw Error(response.error);return response.result;}
const evaluate=script=>request("evaluate",{script});
async function api(domain,action,args={}){
  stage=domain+"."+action;
  // WKWebView collapses rejected promises into a generic JavaScript error.
  // Preserve the application's own rejection for the shared negative tests.
  const result=await evaluate(`window.workflow.api(${JSON.stringify(domain)},${JSON.stringify(action)},${JSON.stringify(args)}).then(value=>({accepted:true,value}),error=>({accepted:false,message:String(error?.message??error)}))`);
  if(!result.accepted)throw Error(result.message);return result.value;
}
async function start(){
  stage="native startup";assert(!child||child.exitCode!==null||child.signalCode!==null);
  fs.rmSync(path.join(data,"test-control.json"),{force:true});
  child=cp.spawn(exe,[],{env:environment,stdio:"ignore"});
  control=await until(()=>fs.existsSync(path.join(data,"test-control.json"))&&JSON.parse(fs.readFileSync(path.join(data,"test-control.json"))),"native startup");
  await until(()=>evaluate("!!window.workflow"),"native renderer");
  await evaluate("(()=>{localStorage.setItem('workflow-language','en');document.querySelector('#dialog-close')?.click();return true})()");
}
async function stop(){
  if(!child)return;
  const current=child;assert.equal(current.exitCode,null,"application must be alive before Quit");assert.equal(current.signalCode,null);
  assert(Array.isArray((await api("app","state")).sessions));assert.equal(await request("quit"),true);
  await until(()=>current.exitCode!==null||current.signalCode!==null,"native quit");
  assert.equal(current.exitCode,0);assert.equal(current.signalCode,null);child=null;
}
const checkedRuns=new Set();
async function mcp(id,method,params={}){
  const leaseFile=path.join(data,"leases",id+".json"),lease=JSON.parse(fs.readFileSync(leaseFile,"utf8"));
  if(!checkedRuns.has(lease.runId)){
    stage="native stdio MCP discovery";
    const requests=[{jsonrpc:"2.0",id:1,method:"initialize",params:{}},{jsonrpc:"2.0",id:2,method:"tools/list",params:{}}];
    const process=cp.spawnSync(exe,["mcp",leaseFile],{env:environment,input:requests.map(r=>JSON.stringify(r)).join("\n")+"\n",encoding:"utf8",timeout:15000,stdio:["pipe","pipe","ignore"]});
    assert.equal(process.status,0);
    const responses=process.stdout.trim().split("\n").map(line=>JSON.parse(line));
    const catalog=responses.find(r=>r.id===2)?.result?.tools;assert(Array.isArray(catalog));
    for(const name of ["jira_read_issue","jira_search","confluence_read_page","confluence_search"]){
      const tool=catalog.find(t=>t.name===name);assert(tool);assert.equal(tool.annotations.readOnlyHint,true);
    }
    const denied=await wire(lease.endpoint,{...lease,token:require("node:crypto").randomUUID(),method,params}).catch(()=>({closed:true}));
    assert(denied.error||denied.closed,"forged native MCP capability must fail");checkedRuns.add(lease.runId);
  }
  return wire(lease.endpoint,{...lease,method,params});
}
// Minimal locator interface runs selectors and real handlers inside WKWebView.
// It does not replace the application DOM, API, network, MCP or native vault.
class Locator {
  constructor(expression="[document]",label="document"){this.expression=expression;this.label=label;}
  find(selector,condition="true",label=selector){return new Locator(`(${this.expression}).flatMap(root=>Array.from(root.querySelectorAll(${JSON.stringify(selector)}))).filter(element=>${condition})`,label);}
  locator(selector){return this.find(selector);}
  getByRole(role,{name,exact}={}){assert.equal(role,"button");assert.equal(exact,true);return this.find("button",`element.textContent.trim()===${JSON.stringify(name)}`,name);}
  getByLabel(name){return this.find("input",`element.getAttribute('aria-label')===${JSON.stringify(name)}`,name);}
  getByText(text,{exact}={}){assert.equal(exact,true);return this.find("*",`element.textContent.trim()===${JSON.stringify(text)}`,text);}
  async ready(){await until(()=>evaluate(`(()=>{const matches=${this.expression};return matches.length===1&&!!matches[0].getClientRects().length&&!matches[0].disabled})()`),"native locator "+this.label);}
  async click(){stage="native button";await this.ready();return evaluate(`(()=>{const element=(${this.expression})[0];element.click();return true})()`);}
  async fill(value){stage="native input";await this.ready();return evaluate(`(()=>{const element=(${this.expression})[0];element.focus();element.value=${JSON.stringify(value)};element.dispatchEvent(new Event('input',{bubbles:true}));element.dispatchEvent(new Event('change',{bubbles:true}));return true})()`);}
  async inputValue(){await this.ready();return evaluate(`(${this.expression})[0].value`);}
  async waitFor(){return this.ready();}
}
async function main(){
  assert(fs.existsSync(fixture));assert(fs.existsSync(exe));
  await require("./atlassian-integration.cjs")({start,stop,api,mcp,until,getPage:()=>new Locator(),temporary,data,project});
}
main().catch(error=>{const line=/atlassian-integration\.cjs:(\d+)/.exec(error.stack||"")?.[1];console.error("FAIL macOS Atlassian acceptance at "+(timeoutStage||stage)+(line?" common line "+line:"")+" ("+error.name+"); raw payload omitted");process.exitCode=1;}).finally(async()=>{
  try {
    if(!child||child.exitCode!==null||child.signalCode!==null){child=null;await start();}
    for(const kind of ["jira","confluence"])await api("atlassian","remove",{kind});
    const statuses=await api("atlassian","status");
    assert(providers.length===3&&["jira","confluence"].every(kind=>statuses[kind].configured===false&&!statuses[kind].unavailable));
    cleanupVerified=true;await stop();
  } catch {console.error("FAIL synthetic native vault cleanup or normal Quit verification");process.exitCode=1;}
  if(child&&child.exitCode===null&&child.signalCode===null)child.kill("SIGTERM");
  if(cleanupVerified)console.log("PASS both synthetic native vault records are absent after cleanup");
  if(!process.exitCode)console.log("PASS macOS Atlassian native acceptance with Codex, Claude Code and Copilot controlled CLIs");
});

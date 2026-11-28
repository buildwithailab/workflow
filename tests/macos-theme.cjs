"use strict";
// Actual WKWebView/PTY tests, with generated profiles and controlled CLIs only.
// OS preference changes are simulated in-page; the native current preference is checked separately.
const fs=require("node:fs"),path=require("node:path"),assert=require("node:assert/strict");
const {Native,until,sleep,page,bind}=require("./macos-chat-permissions.cjs");
const h=new Native("theme"),providers=["codex","claude","copilot"];
let stage="startup";
const evaluate=(fn,arg)=>h.eval(`(${fn.toString()})(${JSON.stringify(arg)??"undefined"})`);
const theme=()=>h.eval("document.documentElement.dataset.theme");
const setTheme=value=>evaluate(value=>window.workflowTheme.setPreference(value),value);
const popup=script=>h.request("evaluate",{view:"popup",script});
const visible=selector=>evaluate(selector=>!!document.querySelector(selector)?.getClientRects().length,selector);
async function click(selector){await page(h).locator(selector).click();}
async function geometry(){
  const boxes=await h.eval(`[".project-rail",".sidebar","main",".inspector"].map(selector=>{const r=document.querySelector(selector).getBoundingClientRect();return {x:r.x,width:r.width,height:r.height}})`);
  for(let i=1;i<boxes.length;i++)assert(boxes[i].x>=boxes[i-1].x+boxes[i-1].width,"panels stay horizontal without overlap");
  assert(boxes[1].width>=180&&boxes[2].width>=400&&boxes[3].width>=310);
  assert(boxes.every(box=>box.height>400));
}
function captured(){
  const folder=path.join(h.data,"test-input");
  return fs.existsSync(folder)?Object.fromEntries(fs.readdirSync(folder).sort().filter(name=>fs.statSync(path.join(folder,name)).isFile()).map(name=>[name,fs.readFileSync(path.join(folder,name)).toString("base64")])):{};
}
async function stable(id,run,conversationId,draft){
  const before=captured();
  for(const value of ["dark","light"]){
    await setTheme(value);assert.equal(await theme(),value);
    const current=await h.api("terminal","attach",{id});
    assert.equal(current.pid,run.pid);assert.equal(current.runId,run.runId);
    assert.equal(current.data,run.data,"theme does not change the terminal output");
    assert.equal((await h.api("app","state")).sessions.find(s=>s.id===id).conversationId,conversationId);
    assert.equal(await page(h).locator("#chat-input").inputValue(),draft);
    assert.deepEqual(await evaluate(id=>{const term=window.workflow.views.get(id).term;return [term.options.theme.background,term.options.minimumContrastRatio];},id),value==="light"?["#ffffff",4.5]:["#0c1015",1]);
  }
  await sleep(120);assert.deepEqual(captured(),before,"theme does not write provider input");
}
async function main(){
  await h.start();assert.equal(await theme(),"dark");await geometry();
  stage="appearance controls";
  await click("#app-settings");
  await h.run("Array.from(document.querySelectorAll('#dialog-body button')).find(b=>b.textContent===tr('Görünüm')).click()");
  assert.equal(await h.eval("document.querySelectorAll('input[name=appearance]').length"),3);
  await click('input[name="appearance"][value="light"]');assert.equal(await theme(),"light");await geometry();
  assert.equal(await h.eval("getComputedStyle(document.body).color"),"rgb(36, 49, 39)");
  await click("#dialog-footer button");
  stage="native current system preference";
  await setTheme("system");
  assert.equal(await theme(),await h.eval("matchMedia('(prefers-color-scheme: light)').matches?'light':'dark'"));
  console.log("PASS native current macOS color preference and manual appearance controls");
  // Exercise the real theme module's change listener without changing global macOS settings.
  stage="simulated system media changes";
  const source=fs.readFileSync(path.join(__dirname,"../ui/theme.js"),"utf8");
  await h.run("window.__themeMedia=new EventTarget();window.__themeMedia.matches=false;window.__nativeMatchMedia=window.matchMedia;window.matchMedia=query=>query==='(prefers-color-scheme: light)'?window.__themeMedia:window.__nativeMatchMedia(query)");
  await h.run(source);
  await h.run("window.matchMedia=window.__nativeMatchMedia");
  for(const light of [true,false]){
    await evaluate(light=>{window.__themeMedia.matches=light;window.__themeMedia.dispatchEvent(new Event("change"));},light);
    assert.equal(await theme(),light?"light":"dark");
  }
  await setTheme("light");await h.run("window.__themeMedia.matches=false;window.__themeMedia.dispatchEvent(new Event('change'))");assert.equal(await theme(),"light");
  console.log("PASS controlled system-media changes follow System and preserve manual preference");
  const folder=path.join(h.directory,"theme-project");fs.mkdirSync(folder);
  const sample=path.join(folder,"sample.js");fs.writeFileSync(sample,"export const spacing = 8;\n");
  for(const marker of ["chat-terminal-fixture","chat-interaction-fixture"])fs.writeFileSync(path.join(folder,marker),"");
  const project=await h.api("app","add_project",{directory:folder}),sessions=[];
  for(const provider of providers){
    stage=provider+" live continuity";
    const s=await h.api("app","create_session",{projectId:project.id,provider,workspaceMode:"direct",tools:false});sessions.push(s);
    const conversationId=bind(h,s);await h.api("history","bind",{id:s.id,conversationId});await h.select(s.id);
    await click("#session-chat-view");await click("#chat-start");
    await until(async()=>(await h.api("chat","interaction",{id:s.id})).ticket,"controlled provider ready");
    const draft="Review the generated spacing.";await page(h).locator("#chat-input").fill(draft);
    const run=await h.api("terminal","attach",{id:s.id});await stable(s.id,run,conversationId,draft);
    stage=provider+" approval after send";await click("#chat-send");
    const approval=await until(async()=>{const prompt=(await h.api("chat","interaction",{id:s.id})).terminalPrompt;return prompt?.kind==="approval"&&prompt.menu&&prompt;},"controlled approval");
    await until(()=>visible("#chat-terminal-prompt"),"visible approval");assert.equal(await visible("#chat-terminal-output"),false);
    const pending=await h.api("terminal","attach",{id:s.id});await stable(s.id,pending,conversationId,"");
    assert.deepEqual((await h.api("chat","interaction",{id:s.id})).terminalPrompt.menu,approval.menu);
    stage=provider+" question after approval";
    await h.run("Array.from(document.querySelectorAll('#chat-terminal-prompt button')).find(b=>/^1 ·/.test(b.textContent)).click()");
    await until(async()=>(await h.api("chat","interaction",{id:s.id})).terminalPrompt?.kind==="question","controlled question");
    await until(()=>h.eval("Array.from(document.querySelectorAll('#chat-terminal-prompt label')).some(n=>n.querySelector('input[type=radio]')&&n.textContent.includes('Only one'))"),"visible question options");
    const question=(await h.api("chat","interaction",{id:s.id})).terminalPrompt.menu;
    await stable(s.id,await h.api("terminal","attach",{id:s.id}),conversationId,"");
    assert.deepEqual((await h.api("chat","interaction",{id:s.id})).terminalPrompt.menu,question);
    assert.equal(await visible("#chat-terminal-output"),false);
    await click("#session-terminal-view");assert.equal(await evaluate(id=>window.workflow.views.get(id).term.options.theme.background,s.id),"#ffffff");
    console.log("PASS "+provider+" native theme, PTY, draft, conversation and collapsed approval/question continuity");
  }
  assert.equal(fs.existsSync(path.join(folder,".git")),false);
  stage="native notification shared store";
  await h.request("minimize");
  await until(async()=>{const state=await h.request("native_state");return state.minimized&&state.popupVisible;},"native notification popup");
  await setTheme("dark");await until(async()=>(await popup("document.documentElement.dataset.theme"))==="dark","actual popup dark");
  await setTheme("light");await until(async()=>(await popup("document.documentElement.dataset.theme"))==="light","actual popup storage light");
  assert.equal(await popup("getComputedStyle(document.querySelector('article')).backgroundColor"),"rgb(239, 243, 237)");
  await popup("window.workflowTheme.setPreference('dark')");await until(async()=>(await theme())==="dark","main follows actual popup storage");
  await h.request("restore");console.log("PASS real native notification WebView shares theme storage in both directions");
  stage="diff palettes";
  fs.writeFileSync(sample,"export const spacing = 12;\n");await h.run("window.workflow.openPanel('changes')");
  await until(()=>visible(".diff-line.insert"),"insert diff");await until(()=>visible(".diff-line.delete"),"delete diff");
  for(const [value,expected] of [["light",[["rgb(40, 94, 59)","rgb(232, 244, 233)"],["rgb(137, 59, 61)","rgb(250, 236, 235)"]]],["dark",[["rgb(192, 229, 172)","rgb(20, 42, 32)"],["rgb(240, 180, 191)","rgb(50, 29, 39)"]]]]){
    await setTheme(value);assert.deepEqual(await h.eval(`[".diff-line.insert",".diff-line.delete"].map(selector=>{const s=getComputedStyle(document.querySelector(selector));return [s.color,s.backgroundColor]})`),expected);
  }
  stage="restart persistence and terminal cleanup";
  await setTheme("light");await h.stop();await h.start();
  assert.equal(await theme(),"light");assert.deepEqual((await h.api("app","state")).active,[]);
  for(const s of sessions)assert.equal((await h.api("terminal","attach",{id:s.id})).exited,true);
  for(const provider of providers){
    stage=provider+" controlled account login palette";await setTheme("light");
    await evaluate(provider=>{openProviderLogin(provider);return true;},provider);
    await until(()=>h.eval("Array.from(document.querySelectorAll('#dialog p')).some(p=>p.textContent===tr('Sağlayıcının giriş adımlarını takip et.'))"),"controlled login started");
    assert.equal(await h.eval("getComputedStyle(document.querySelector('.login-terminal .xterm-viewport')).backgroundColor"),"rgb(255, 255, 255)");
    await setTheme("dark");await until(()=>h.eval("getComputedStyle(document.querySelector('.login-terminal .xterm-viewport')).backgroundColor==='rgb(12, 16, 21)'"),"controlled login dark");
    await click("#dialog-footer button");
  }
  await h.stop();
  console.log("PASS native diff palettes, restart persistence, no provider restart, owned PTY exit and controlled login palette");
}
main().catch(error=>{console.error("FAIL native theme at "+stage+" ("+error.name+"); generated payload omitted");process.exitCode=1;}).finally(()=>h.teardown());

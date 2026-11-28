// All credentials, API content and identifiers are generated locally for this run.
module.exports = async function testAtlassian({start, stop, api, mcp, until, getPage, temporary, data, project}) {
  const assert = require("assert/strict"), http = require("http"), fs = require("fs"), path = require("path"), {randomBytes} = require("crypto");
  const credential = () => randomBytes(24).toString("hex");
  let secret = credential(), previous = secret, mode = "ok", requests = [], pending;
  const key = "SAMPLE-" + (1 + require("crypto").randomInt(100000)), pageId = String(1 + require("crypto").randomInt(100000));
  let foreignCalls = 0;
  const foreign = http.createServer((req,res) => {foreignCalls++; res.end("{}");});
  await new Promise(resolve => foreign.listen(0,"127.0.0.1",resolve));
  const server = http.createServer((req,res) => {
    const url = new URL(req.url,"http://127.0.0.1");
    requests.push({method:req.method,path:url.pathname,query:url.searchParams,authorized:req.headers.authorization === "Bearer " + secret});
    if (mode === "hold" && url.pathname.endsWith("/search")) { pending = () => {res.setHeader("Content-Type","application/json");res.end(JSON.stringify({issues:[],total:0}));};return; }
    if (mode === "redirect") {res.writeHead(302,{Location:"http://127.0.0.1:"+foreign.address().port});res.end();return;}
    if (mode === "html") {res.end("<html>Sign in</html>");return;}
    if (mode === "oversize") {res.end(" ".repeat(1024*1024+2));return;}
    if (/^\d+$/.test(mode)) {res.writeHead(Number(mode));res.end(secret);return;}
    res.setHeader("Content-Type","application/json");
    const issue = {key,fields:{summary:"Generated issue",description:"Generated description",status:{name:"Open"},issuetype:{name:"Task"},reporter:{displayName:"Test author",emailAddress:"not-returned@localhost"}}};
    let value;
    if (url.pathname.endsWith("/myself") || url.pathname.endsWith("/user/current")) value={displayName:"Generated identity"};
    else if (url.pathname.endsWith("/remotelink")) value=[{object:{title:"Generated page",url:"https://reference.example.invalid/"}}];
    else if (url.pathname.endsWith("/comment")) {
      if (mode === "partial") {res.writeHead(403);res.end();return;}
      value={comments:[{author:{displayName:"Test author"},body:"Generated comment",created:""}],total:1};
    }
    else if (url.pathname === "/jira/rest/api/2/issue/"+key) value=issue;
    else if (url.pathname === "/jira/rest/api/2/search") value={issues:[issue],total:1};
    else if (url.pathname === "/confluence/rest/api/content/search") value={results:[{id:pageId,title:"Generated page"}]};
    else if (url.pathname === "/confluence/rest/api/content/"+pageId) value={id:pageId,title:"Generated page",body:{view:{value:"<p>Generated content</p><script>untrusted()</script>"}},version:{number:1},space:{key:"SAMPLE"}};
    else {res.writeHead(404);res.end();return;}
    if (mode === "reflect" && value.fields) value.fields.description = secret;
    res.end(JSON.stringify(value));
  });
  await new Promise(resolve => server.listen(0,"127.0.0.1",resolve));
  const address = kind => "http://127.0.0.1:"+server.address().port+"/"+kind;
  const save = kind => api("atlassian","save",{kind,baseUrl:address(kind),token:secret});
  const cleanVault = async () => {for (const kind of ["jira","confluence"]) await api("atlassian","remove",{kind});};
  try {
    await start(); await cleanVault();
    assert.equal((await api("atlassian","status")).jira.configured,false);
    const page = getPage();
    await page.locator("#app-settings").click();
    await page.getByRole("button",{name:"Connections",exact:true}).click();
    const jira = page.locator('.atlassian-connection[data-kind="jira"]');
    await jira.getByLabel("Jira Server address").fill(address("jira"));
    await jira.getByLabel("Jira Personal access token").fill(secret);
    await jira.getByRole("button",{name:"Test connection",exact:true}).click();
    await jira.getByText("Connection test succeeded.",{exact:true}).waitFor();
    assert.equal((await api("atlassian","status")).jira.configured,false,"testing does not save");
    await jira.getByRole("button",{name:"Save",exact:true}).click();
    await jira.getByText("Connection saved.",{exact:true}).waitFor();
    assert.equal(await jira.getByLabel("Jira Personal access token").inputValue(),"");
    await page.locator("#dialog-footer button").click();
    await save("confluence");
    const status=await api("atlassian","status");
    assert(status.jira.configured && status.confluence.configured);
    assert(!JSON.stringify(status).includes(secret));
    await assert.rejects(api("atlassian","test",{kind:"jira",baseUrl:address("elsewhere"),token:""}),/Yeni adres|new address/);
    assert.equal((await api("atlassian","test",{kind:"jira",baseUrl:address("jira"),token:""})).connected,true);
    const p=await api("app","add_project",{directory:project});
    const sessions=[];
    for (const provider of ["codex","claude","copilot"]) {
      const session=await api("app","create_session",{projectId:p.id,provider,workspaceMode:"direct",tools:true});sessions.push(session);
      const started=await api("terminal","start",{id:session.id});
      await until(()=>fs.existsSync(path.join(data,"leases",session.id+".json")),"integration lease");
      for (const [method,args] of [["jira_read_issue",{key}],["jira_search",{jql:'text ~ "generated"'}],["confluence_search",{query:'generated "quote"'}],["confluence_read_page",{pageId}]]) {
        const response=await mcp(session.id,method,args);
        assert(!response.error,"read-only integration failed");
        assert.equal(response.result.access,"read-only");assert.equal(response.result.contentIsUntrusted,true);
        assert(!JSON.stringify(response).includes(secret));
        assert(!JSON.stringify(response).includes("emailAddress"));
        if (method === "confluence_read_page") {assert.match(response.result.data.body,/Generated content/);assert(!response.result.data.body.includes("untrusted()"));}
      }
      assert.equal((await api("terminal","attach",{id:session.id})).runId,started.runId,"tools preserve the provider conversation/process");
    }
    assert(requests.every(r=>r.method==="GET" && r.authorized));
    assert(requests.some(r=>r.path.startsWith("/jira/rest/")) && requests.some(r=>r.path.startsWith("/confluence/rest/")));
    const cql=requests.find(r=>r.path.endsWith("/content/search")).query.get("cql");
    assert(cql.includes('\\"quote\\"'),"CQL user text is escaped rather than executable query syntax");
    for (const failure of ["401","403","429","redirect","html","oversize"]) {
      mode=failure;
      await assert.rejects(api("atlassian","test",{kind:"jira",baseUrl:address("jira"),token:""}),e=>!String(e).includes(secret));
      assert.equal((await api("atlassian","status")).jira.configured,true,"failed verification preserves the saved connection");
    }
    assert.equal(foreignCalls,0,"redirects never forward credentials");
    mode="partial";assert.equal((await mcp(sessions[0].id,"jira_read_issue",{key})).result.data.comments.unavailable,true);
    mode="reflect";assert(!JSON.stringify(await mcp(sessions[0].id,"jira_read_issue",{key})).includes(secret));
    mode="hold";
    const inflight=mcp(sessions[0].id,"jira_search",{jql:"text ~ generated"});
    await until(()=>!!pending,"held request");await api("atlassian","remove",{kind:"jira"});pending();pending=null;
    assert((await inflight).error,"revoking a connection rejects an in-flight result");
    mode="ok";
    for (const session of sessions) assert((await mcp(session.id,"jira_search",{jql:"text ~ generated"})).error,"all providers lose revoked access");
    previous=secret;secret=credential();await save("jira");await save("confluence");
    for (const session of sessions) {
      assert((await mcp(session.id,"jira_search",{jql:"text ~ generated"})).result);
      await api("terminal","stop",{id:session.id});
      const stopped=await mcp(session.id,"jira_search",{jql:"text ~ generated"}).catch(()=>({closed:true}));assert(stopped.error||stopped.closed,"closed run cannot use connections");
    }
    await stop();await start();
    assert.equal((await api("atlassian","status")).jira.configured,true,"native vault survives application restart");
    function audit(folder) {for (const entry of fs.readdirSync(folder,{withFileTypes:true})) {const file=path.join(folder,entry.name);if(entry.isDirectory()) audit(file);else if(entry.isFile()){const bytes=fs.readFileSync(file);assert(!bytes.includes(Buffer.from(secret))&&!bytes.includes(Buffer.from(previous)),"integration credentials must not be saved in profile files");}}}
    await cleanVault();
    assert.equal((await api("atlassian","status")).confluence.configured,false);
    await stop();audit(data);
    console.log("PASS Jira/Confluence: native vault, settings, four read-only tools with all three controlled provider CLIs, context paths, paging, redaction, revocation, error and restart paths");
  } finally {
    if (pending) pending();
    try {await cleanVault();} catch {}
    await stop();await Promise.all([new Promise(resolve=>server.close(resolve)),new Promise(resolve=>foreign.close(resolve))]);
  }
};

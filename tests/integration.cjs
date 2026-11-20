const fs = require("fs"),
  path = require("path"),
  os = require("os"),
  net = require("net"),
  http = require("http"),
  cp = require("child_process"),
  assert = require("assert/strict"),
  { createHash, randomBytes } = require("crypto");
const root = path.resolve(__dirname, ".."),
  exe =
    process.env.WORKFLOW_TEST_EXE ||
    path.join(root, "target/debug/workflow.exe");
const playwright = require(
  process.env.WORKFLOW_PLAYWRIGHT || "playwright-core",
);
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "workflow-test-")),
  data = path.join(temporary, "data"),
  project = path.join(temporary, "sample"),
  gitProject = path.join(temporary, "repository");
const usageRoot = path.join(data, "usage");
const fixtureCredential = randomBytes(24).toString("hex");
const managedFixtureCredential = randomBytes(24).toString("hex");
fs.mkdirSync(usageRoot, { recursive: true });
const usageAuth = {
  loggedIn: true,
  authMethod: "claude.ai",
  email: "fixture@localhost",
  orgId: "fixture",
  configDirectory: usageRoot,
};
fs.writeFileSync(
  path.join(usageRoot, "auth-status.json"),
  JSON.stringify(usageAuth),
);
fs.writeFileSync(
  path.join(usageRoot, ".credentials.json"),
  JSON.stringify({
    claudeAiOauth: {
      accessToken: fixtureCredential,
      expiresAt: Date.now() + 86400000,
    },
  }),
);
let usageStatus = 200,
  usageSwitch = false,
  usageRequests = 0;
fs.mkdirSync(project);
fs.mkdirSync(gitProject);
fs.writeFileSync(path.join(project, "sample.txt"), "alpha\nbeta\n");
const fixtureExe = path.join(temporary, "provider-fixture.exe");
cp.execFileSync(
  "rustc",
  [
    path.join(root, "tests/provider-fixture.rs"),
    "-o",
    fixtureExe,
    "-C",
    "target-feature=+crt-static",
  ],
  {
    cwd: process.env.WORKFLOW_TEST_WORKDIR || root,
    windowsHide: true,
    stdio: "pipe",
  },
);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(fn, label, timeout = 60000) {
  const end = Date.now() + timeout;
  let last;
  while (Date.now() < end) {
    try {
      const r = await fn();
      if (r) return r;
    } catch (e) {
      last = e;
    }
    await sleep(150);
  }
  throw Error("Timeout: " + label + " " + (last?.message || ""));
}
function freePort() {
  return new Promise((resolve) => {
    const s = net.createServer().listen(0, "127.0.0.1", () => {
      const port = s.address().port;
      s.close(() => resolve(port));
    });
  });
}
let child,
  browser,
  page,
  server,
  pids = [],
  calls = 0;
async function start(missingProviders = "", profile = data, environment = {}) {
  const port = await freePort();
  const env = {
    ...process.env,
    WORKFLOW_DATA_DIR: profile,
    CODEX_HOME: path.join(profile, "test-codex-home"),
    CLAUDE_CONFIG_DIR: path.join(profile, "test-claude-home"),
    COPILOT_HOME: path.join(profile, "test-copilot-home"),
    WORKFLOW_TEST_MODE: "1",
    WORKFLOW_TEST_PROVIDER: fixtureExe,
    WORKFLOW_TEST_RPC_EXE: exe,
    WORKFLOW_TEST_MISSING_PROVIDERS: missingProviders,
    WORKFLOW_TEST_USAGE_DIR: usageRoot,
    WORKFLOW_TEST_MANAGED_CREDENTIAL: managedFixtureCredential,
    WORKFLOW_TEST_USAGE_URL:
      "http://127.0.0.1:" + server.address().port + "/usage",
    WORKFLOW_TEST_INPUT_DIR: path.join(data, "test-input"),
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: "--remote-debugging-port=" + port,
    ...environment,
  };
  child = cp.spawn(exe, [], {
    cwd: process.env.WORKFLOW_TEST_WORKDIR || root,
    env,
    windowsHide: true,
    stdio: "ignore",
  });
  let errors = "";
  child.on("error", (e) => {
    errors = e.message;
  });
  await until(() => {
    if (errors || child.exitCode !== null)
      throw Error(errors || "Application exited: " + child.exitCode);
    return fs.existsSync(path.join(profile, "ready.json"));
  }, "startup");
  browser = await until(
    () => playwright.chromium.connectOverCDP("http://127.0.0.1:" + port),
    "CDP",
  );
  page = await until(async () => {
    const pages = browser.contexts().flatMap((c) => c.pages());
    for (const p of pages)
      if (await p.evaluate(() => !!window.workflow).catch(() => false))
        return p;
    return null;
  }, "Workflow UI");
  page.on("pageerror", (e) => console.error("UI ERROR", e.message));
  return { port, errors };
}
async function api(domain, action, args = {}) {
  calls++;
  return page.evaluate(
    ({ domain, action, args }) => window.workflow.api(domain, action, args),
    { domain, action, args },
  );
}
async function mcp(sid, method, params = {}, override) {
  const lease = JSON.parse(
    fs.readFileSync(path.join(data, "leases", sid + ".json")),
  );
  return new Promise((resolve, reject) => {
    const socket = net.connect(
      Number(lease.endpoint.split(":").pop()),
      "127.0.0.1",
      () =>
        socket.write(
          JSON.stringify({ ...lease, method, params, ...override }) + "\n",
        ),
    );
    let raw = "";
    socket.setTimeout(25000, () => socket.destroy(Error("MCP timeout")));
    socket.on("data", (b) => {
      raw += b;
      if (raw.includes("\n")) {
        socket.end();
        try {
          resolve(JSON.parse(raw.trim()));
        } catch (e) {
          reject(e);
        }
      }
    });
    socket.on("error", reject);
    socket.on("end", () => {
      if (!raw) resolve({ closed: true });
    });
  });
}
async function stdioMcp(sid, unavailable = false) {
  let leaseFile = path.join(data, "leases", sid + ".json");
  if (unavailable) {
    const lease = JSON.parse(fs.readFileSync(leaseFile, "utf8"));
    lease.endpoint = "127.0.0.1:" + await freePort();
    leaseFile = path.join(temporary, "unavailable-lease.json");
    fs.writeFileSync(leaseFile, JSON.stringify(lease));
  }
  const c = cp.spawn(exe, ["mcp", leaseFile], {
    env: { ...process.env },
    windowsHide: true,
    stdio: ["pipe", "pipe", "ignore"],
  });
  let buffer = "",
    messages = [];
  c.stdout.on("data", (b) => {
    buffer += b;
    let end;
    while ((end = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, end);
      buffer = buffer.slice(end + 1);
      try {
        messages.push(JSON.parse(line));
      } catch {}
    }
  });
  for (const request of [
    { id: 1, method: "initialize", params: {} },
    { id: 2, method: "tools/list", params: {} },
    {
      id: 3,
      method: "tools/call",
      params: { name: "session_catalog", arguments: {} },
    },
    { id: 4, method: "ping" },
  ])
    c.stdin.write(JSON.stringify({ jsonrpc: "2.0", ...request }) + "\n");
  try {
    await until(() => {
      if (c.exitCode !== null)
        throw Error(
          "MCP process exited: " +
            c.exitCode +
            "; responses: " +
            messages.map((m) => m.id).join(","),
        );
      return messages.length >= 4;
    }, "MCP stdio");
  } catch (error) {
    c.stdin.end();
    c.kill();
    throw error;
  }
  assert.equal(
    messages.find((v) => v.id === 1).result.serverInfo.name,
    "workflow",
  );
  assert(
    messages
      .find((v) => v.id === 2)
      .result.tools.some((t) => t.name === "browser_action"),
  );
  const call = messages.find((v) => v.id === 3).result;
  if (unavailable) {
    assert.equal(call.isError, true);
    assert.match(call.content[0].text, /bağlantısı kurulamadı/);
  } else {
    assert.equal(JSON.parse(call.content[0].text).length, 1);
  }
  assert.deepEqual(messages.find((v) => v.id === 4).result, {});
  c.stdin.end();
  await until(() => c.exitCode !== null, "MCP exit");
}
async function stop() {
  if (child && child.exitCode === null) {
    try {
      await api("app", "quit");
    } catch {}
    await until(() => child.exitCode !== null, "window close", 10000);
  }
  try {
    await browser?.close();
  } catch {}
  browser = null;
  page = null;
  child = null;
}
async function testCopilotNpm() {
  const base = path.join(temporary, "copilot-npm");
  const roaming = path.join(base, "roaming");
  const defaultShim = path.join(roaming, "npm", "copilot.cmd");
  const prefix = path.join(base, "global dizin " + String.fromCodePoint(0x131));
  const customShim = path.join(prefix, "copilot.cmd");
  const bin = path.join(base, "bin");
  const launcher = "@echo off\r\necho Copilot npm fixture\r\n";
  for (const dir of [bin, prefix, path.dirname(defaultShim), path.join(base, "local")])
    fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(defaultShim, launcher);
  fs.writeFileSync(customShim, launcher);
  fs.writeFileSync(path.join(bin, "npm.cmd"), '@echo off\r\nnode "%~dp0npm-fixture.cjs" %*\r\nexit /b %errorlevel%\r\n');
  fs.writeFileSync(path.join(bin, "npm-fixture.cjs"), [
    'const fs = require("node:fs"), path = require("node:path");',
    'const args = process.argv.slice(2).join(" ");',
    'const prefix = process.env.NPM_CONFIG_PREFIX;',
    'if (args === "prefix -g") console.log(prefix);',
    'else if (args === "install -g @github/copilot")',
    '  fs.writeFileSync(path.join(prefix, "copilot.cmd"), ' + JSON.stringify(launcher) + ');',
    'else process.exitCode = 1;',
  ].join("\n"));
  await start("codex,claude", path.join(base, "test-profile"), {
    PATH: [
      bin,
      path.dirname(process.execPath),
      path.join(process.env.SystemRoot, "System32"),
      path.join(process.env.SystemRoot, "System32", "WindowsPowerShell", "v1.0"),
    ].join(path.delimiter),
    APPDATA: roaming,
    LOCALAPPDATA: path.join(base, "local"),
    NPM_CONFIG_PREFIX: prefix,
    WORKFLOW_TEST_PROVIDER: undefined,
    WORKFLOW_TEST_RPC_EXE: undefined,
  });
  const dialog = page.locator("#dialog");
  await dialog.getByRole("heading", { name: "Provider setup" }).waitFor();
  await dialog.getByRole("button", { name: "Skip", exact: true }).click();
  const copilot = async () => (await api("app", "providers")).find(p => p.id === "copilot");
  assert.equal((await copilot()).executable, defaultShim);
  fs.unlinkSync(defaultShim);
  assert.equal((await copilot()).executable, customShim);
  const login = await api("provider_login", "start", { provider: "copilot" });
  await until(async () => {
    const result = await api("provider_login", "poll", { runId: login.runId, since: 0 });
    return Buffer.from(result.data, "base64").toString().includes("Copilot npm fixture");
  }, "npm Copilot launcher runs");
  await api("provider_login", "close", { runId: login.runId });
  fs.unlinkSync(customShim);
  assert.equal((await copilot()).installed, false);
  const options = (await api("app", "provider_install_options")).copilot;
  assert.equal(options.find(method => method.id === "winget").available, false);
  const nodeSupported = Number(process.versions.node.split(".")[0]) >= 22;
  assert.equal(options.find(method => method.id === "npm").available, nodeSupported);
  await assert.rejects(api("app", "provider_install", { provider: "copilot", method: "npm" }), /önce onayla/);
  await page.locator("#app-settings").click();
  await dialog.getByRole("button", { name: "Provider setup", exact: true }).click();
  await dialog.getByRole("button", { name: "Rescan", exact: true }).click();
  await dialog.locator(".setup-row").filter({ hasText: "GitHub Copilot" })
    .getByText(/Not installed/).waitFor();
  await dialog.getByRole("button", { name: "Next", exact: true }).click();
  const npmRow = dialog.locator(".setup-command").filter({ hasText: "npm.cmd install -g @github/copilot" });
  await npmRow.waitFor();
  const install = npmRow.getByRole("button", { name: "Install", exact: true });
  assert.equal(await install.isEnabled(), nodeSupported);
  if (nodeSupported) {
    await install.click();
    await dialog.getByRole("button", { name: "Install", exact: true }).click();
    await until(() => fs.existsSync(customShim), "confirmed npm installation");
    assert.equal((await copilot()).executable, customShim);
  }
  await stop();
  console.log("PASS Copilot npm discovery, launcher and installation option without WinGet");
}

async function testCodexChat() {
  const profile = path.join(temporary, "chat-test-profile");
  const folder = path.join(temporary, "chat-project");
  fs.mkdirSync(folder, { recursive: true });
  fs.writeFileSync(path.join(folder, "chat-terminal-fixture"), "");
  fs.writeFileSync(path.join(folder, "preferences.txt"), "enabled=true\n");
  await start("claude,copilot", profile);
  const project = await api("app", "add_project", { directory: folder });
  const s = await api("app", "create_session", {
    projectId: project.id, provider: "codex", workspaceMode: "direct",
    name: "Notification preferences", tools: false,
  });
  const logs = path.join(profile, "test-codex-home", "sessions");
  fs.mkdirSync(logs, { recursive: true });
  const log = path.join(logs, "rollout-chat-test.jsonl");
  const event = (type, fields = {}) => ({ type: "event_msg", payload: { type, ...fields } });
  const response = (type, fields = {}) => ({ type: "response_item", payload: { type, ...fields } });
  const append = (...entries) => fs.appendFileSync(log, entries.map(x => JSON.stringify(x) + "\n").join(""));
  append(
    { type: "session_meta", payload: { id: "chat-test-thread", cwd: s.directory, timestamp: new Date().toISOString() } },
    event("task_started", { turn_id: "first" }),
    event("user_message", { message: "Fix the notification preference." }),
    response("message", { role: "assistant", phase: "commentary", content: [{ text: "Checking the saved value" }] }),
    response("function_call", { name: "apply_patch", call_id: "patch" }),
    response("message", { role: "assistant", phase: "final_answer", content: [{ text: "**Fixed.**\n\n- The saved value is preserved.\n\n\x60\x60\x60ts\nconst enabled = saved ?? true;\n\x60\x60\x60" }] }),
    event("task_complete"),
  );
  await api("history", "bind", { id: s.id, conversationId: "chat-test-thread" });
  fs.writeFileSync(path.join(folder, "preferences.txt"), "enabled=false\n");
  const setup = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
  if (await setup.isVisible()) await setup.click();
  await page.evaluate(async id => {
    await window.workflow.refresh();
    window.workflow.selectSession(id, true);
  }, s.id);
  await page.locator("#session-chat-view").click();
  const chat = page.locator("#chat-view");
  await chat.getByText("Fixed.", { exact: true }).waitFor();
  assert.equal(await chat.locator("pre code").textContent(), "const enabled = saved ?? true;\n");
  const steps = chat.locator("details");
  assert.equal(await steps.evaluate(node => node.open), false);
  await steps.locator("summary").click();
  await steps.getByText("apply_patch", { exact: true }).waitFor();
  await page.locator("#panel-changes").getByRole("button", { name: /preferences\.txt/ }).waitFor();
  assert.deepEqual((await api("app", "state")).active, [], "opening Chat does not start a terminal");

  const input = chat.locator("textarea");
  const screenshot = fs.readFileSync(path.join(root, "icons", "icon.png"));
  const pasteScreenshot = selector => page.locator(selector).evaluate((node, bytes) => {
    const data = new DataTransfer();
    data.items.add(new File([Uint8Array.from(bytes)], "screenshot.png", { type: "image/png" }));
    node.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }));
  }, [...screenshot]);
  const dropFiles = (selector, paths) => page.locator(selector).evaluate((node, paths) => {
    const box = node.getBoundingClientRect();
    return window.__TAURI__.event.emit("tauri://drag-drop", {
      paths,
      position: { x: (box.left + box.width / 2) * devicePixelRatio, y: (box.top + box.height / 2) * devicePixelRatio },
    });
  }, paths);
  const screenshots = path.join(profile, "attachments", s.id);
  await pasteScreenshot("#chat-input");
  const chips = page.locator("#chat-images button");
  await chips.filter({ hasText: "Screenshot" }).waitFor();
  const [capture] = fs.readdirSync(screenshots);
  assert.deepEqual(fs.readFileSync(path.join(screenshots, capture)), screenshot);
  await chips.filter({ hasText: "Screenshot" }).click();
  await until(() => !fs.existsSync(path.join(screenshots, capture)), "removing an unsent screenshot deletes its local copy");
  const dropped = [path.join(folder, "preferences.txt"), path.join(root, "icons", "icon.png")];
  await dropFiles("#chat-view", dropped);
  for (const file of dropped) {
    const chip = chips.filter({ hasText: path.basename(file) });
    await chip.waitFor();
    await chip.click();
  }
  const prompt = "Retest the preference.\nInclude the reload case.";
  await input.fill(prompt);
  const modelMenu = page.locator("#chat-model-menu");
  await page.locator("#chat-model").click();
  await modelMenu.getByRole("button", { name: "Fixture Chat", exact: true }).click();
  const selectedModel = async () => (await api("app", "state")).sessions.find(row => row.id === s.id);
  await until(async () => (await selectedModel()).model === "fixture-chat", "model selection saved before startup");
  await page.locator("#chat-effort").click();
  await modelMenu.getByRole("button", { name: "High", exact: true }).click();
  await until(async () => (await selectedModel()).effort === "high", "reasoning effort saved before startup");
  assert.deepEqual((await api("app", "state")).active, [], "choosing settings does not start Codex");
  assert.equal(await input.inputValue(), prompt);
  await page.locator("#chat-start").click();
  await until(async () => !!(await api("chat", "read", { id: s.id })).ticket, "native Codex composer ready", 12000);
  await page.evaluate(id => {
    collectChatAttachments(id, () => new Promise(resolve => { window.finishTestAttachment = () => resolve([]); }));
    document.querySelector("#chat-composer").requestSubmit();
  }, s.id);
  assert.equal(await page.locator("#chat-send").isDisabled(), true);
  assert(!fs.existsSync(path.join(data, "test-input", "codex-chat.bin")));
  await page.evaluate(() => window.finishTestAttachment());
  await until(async () => !await page.locator("#chat-send").isDisabled(), "attachment selection finished");
  await page.evaluate(id => {
    window.workflow.views.get(id).inputDraft = "obsolete terminal estimate";
    document.querySelector("#chat-input").dispatchEvent(new Event("input"));
  }, s.id);
  assert.equal(await page.locator("#chat-send").isDisabled(), false);
  await page.evaluate(id => { window.workflow.views.get(id).inputDraft = ""; }, s.id);
  const runId = (await api("terminal", "attach", { id: s.id })).runId;
  const ticketBeforeModel = (await api("chat", "read", { id: s.id })).ticket;
  await assert.rejects(api("models", "apply", { id: s.id, surface: "chat", model: "fixture-chat", effort: "ultra", ...ticketBeforeModel }), /Düşünme seviyesi geçersiz/);
  await page.locator("#chat-effort").click();
  await modelMenu.getByRole("button", { name: "Medium", exact: false }).click();
  await until(async () => (await selectedModel()).effort === "medium", "live reasoning effort confirmed by Codex");
  await until(async () => !!(await api("chat", "read", { id: s.id })).ticket, "Codex ready after model change");
  assert.equal((await api("terminal", "attach", { id: s.id })).runId, runId);
  assert.equal(await input.inputValue(), prompt);
  const submitted = path.join(data, "test-input", "codex-chat.bin");
  await page.locator("#session-terminal-view").click();
  assert(await chat.isHidden());
  await page.locator("#session-chat-view").click();
  assert.equal(await input.inputValue(), prompt);
  assert.equal((await api("terminal", "attach", { id: s.id })).runId, runId);
  assert(!fs.existsSync(submitted), "view switching sends no terminal input");
  const ticket = (await api("chat", "read", { id: s.id })).ticket;
  await assert.rejects(api("chat", "send", { id: s.id, ...ticket, runId: "stale", text: prompt }), /çalıştırması değişti/);
  await assert.rejects(api("chat", "send", { id: s.id, ...ticket, text: "/new" }), /kabuk komutlarını/);
  await chat.getByRole("button", { name: "Send", exact: true }).click();
  await until(() => fs.existsSync(submitted), "same Codex process received the chat input", 12000);
  const received = fs.readFileSync(submitted);
  assert.equal(received[received.length - 1], 13);
  assert.match(received.toString(), /^\x1b\[200~Retest the preference\.\x1b\[201~/);
  assert.match(received.toString(), /\x1b\[200~Include the reload case\.\x1b\[201~\r$/);
  await until(async () => {
    const snapshot = await api("terminal", "attach", { id: s.id });
    return Buffer.from(snapshot.data, "base64").toString().includes("Working (test)");
  }, "Codex displays a running task next to its composer");
  assert.equal((await api("chat", "read", { id: s.id })).ticket, null);
  append(
    event("task_started", { turn_id: "second" }),
    response("message", {
      role: "user",
      content: [{ text: "Environment context" }, { text: prompt }],
      internal_chat_message_metadata_passthrough: {
        content_item_kinds: ["environments.environment_context", "user.text"],
        content_item_metadata: [{ provenance: { type: "harness" } }, { provenance: { type: "user" } }],
      },
    }),
    event("task_complete", { last_agent_message: "Reload test passed." }),
  );
  await chat.getByText("Reload test passed.", { exact: true }).waitFor();
  assert.equal(await chat.locator(".chat-prompt").count(), 2);
  assert.equal(await chat.locator(".chat-prompt").last().textContent(), prompt);
  assert.equal((await api("terminal", "attach", { id: s.id })).runId, runId);
  await page.locator("#session-terminal-view").click();
  const terminal = `.session-card[data-id="${s.id}"] .terminal-host`;
  await page.locator(terminal).waitFor();
  await pasteScreenshot(terminal);
  await dropFiles(terminal, dropped);
  await page.waitForFunction(id => {
    const draft = window.workflow.views.get(id)?.inputDraft;
    return draft?.includes(".png") && draft.includes("preferences.txt") && draft.includes("icon.png");
  }, s.id);
  const [terminalCapture] = fs.readdirSync(screenshots);
  assert.deepEqual(fs.readFileSync(path.join(screenshots, terminalCapture)), screenshot);
  assert.deepEqual(fs.readFileSync(submitted), received, "pasting or dropping does not submit the prompt");
  await api("terminal", "stop", { id: s.id, runId });
  await until(async () => (await api("terminal", "attach", { id: s.id })).exited, "Codex fixture stopped");
  await page.locator("#session-chat-view").click();
  await input.fill("First conversation only");
  await pasteScreenshot("#chat-input");
  await chips.filter({ hasText: "Screenshot" }).waitFor();
  fs.writeFileSync(path.join(logs, "rollout-second-chat.jsonl"), JSON.stringify({
    type: "session_meta", payload: { id: "second-chat-test-thread", cwd: s.directory, timestamp: new Date().toISOString() },
  }) + "\n");
  await api("history", "bind", { id: s.id, conversationId: "second-chat-test-thread" });
  await page.evaluate(() => window.workflow.refresh());
  assert.equal(await input.inputValue(), "");
  assert.equal(await chips.count(), 0, "rebinding drops unsent attachments from the prior conversation");
  const leaked = await page.evaluate(async id => {
    const current = window.workflow.state().sessions.find(row => row.id === id);
    current.conversationId = null;
    renderChatView();
    let complete;
    const pending = collectChatAttachments(id, () => new Promise(resolve => { complete = resolve; }));
    current.conversationId = "first-bound-thread";
    renderChatView();
    current.conversationId = "replacement-thread";
    renderChatView();
    complete([{ name: "sample.png", reference: "sample.png", kind: "image" }]);
    await pending;
    return chatViews.get(id).images.length;
  }, s.id);
  assert.equal(leaked, 0, "an attachment started before the first bind cannot reach a later conversation");
  await page.evaluate(() => window.workflow.refresh());
  await stop();
  console.log("PASS Codex Chat and Terminal share one run, render structured messages and steps, and review files in the inspector");
}

async function testAdditionalProviderChats() {
  for (const provider of ["claude", "copilot"]) {
    const claude = provider === "claude";
    const profile = path.join(temporary, provider + "-chat-profile");
    const folder = path.join(temporary, provider + "-chat-workspace");
    fs.mkdirSync(folder, { recursive: true });
    fs.writeFileSync(path.join(folder, "chat-terminal-fixture"), "");
    const picture = path.join(temporary, provider + " sample.png");
    fs.copyFileSync(path.join(root, "icons", "icon.png"), picture);
    await start("", profile);
    const p = await api("app", "add_project", { directory: folder });
    const s = await api("app", "create_session", { projectId: p.id, provider, workspaceMode: "direct", tools: false });
    const logs = claude ? path.join(profile, "test-claude-home/projects/example")
      : path.join(profile, "test-copilot-home/session-state", s.id);
    fs.mkdirSync(logs, { recursive: true });
    const log = path.join(logs, claude ? `${s.id}.jsonl` : "events.jsonl");
    const entry = (kind, content, extra = {}) => claude
      ? { type: kind, sessionId: s.id, cwd: s.directory, message: { content }, ...extra }
      : { type: kind, data: { content, ...extra } };
    const append = (...rows) => fs.appendFileSync(log, rows.map(row => JSON.stringify(row) + "\n").join(""));
    if (!claude) append(entry("session.start", "", { sessionId: s.id, context: { cwd: s.directory } }));
    append(entry(claude ? "user" : "user.message", "Check the selected setting."));
    append(claude
      ? entry("assistant", [{ type: "thinking", thinking: "Private thought" }, { type: "tool_use", name: "Edit", id: "tool-1", input: { old: "Tool contents" } }])
      : entry("tool.execution_start", "", { toolName: "edit", toolCallId: "tool-1", arguments: { old: "Tool contents" } }));
    append(entry(claude ? "assistant" : "assistant.message", [{ type: "text", text: "The setting is ready." }]));
    await api("history", "bind", { id: s.id, conversationId: s.id });
    const setup = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
    if (await setup.isVisible()) await setup.click();
    await page.evaluate(async id => { await window.workflow.refresh(); window.workflow.selectSession(id, true); }, s.id);
    await page.locator("#session-chat-view").click();
    await page.locator("#chat-view").getByText("The setting is ready.").waitFor();
    await page.locator("#chat-view details summary").click();
    assert.equal(await page.locator("#chat-view").getByText("Tool contents").count(), 0);
    const attach = await api("chat", "image_reference", { id: s.id, path: picture });
    assert.equal(attach.name, provider + " sample.png");
    assert.match(attach.reference, /sample\.png"$/);
    await assert.rejects(api("chat", "image_reference", { id: s.id, path: "relative.png" }), /PNG/);
    const approval = page.locator("#chat-approval");
    if (claude) await approval.selectOption("plan");
    await page.locator("#chat-start").click();
    await until(async () => !!(await api("chat", "read", { id: s.id })).ticket, provider + " idle composer");
    if (!claude) {
      assert.equal((await api("chat", "read", { id: s.id })).liveApprovalMode, "manual");
      await until(async () => await approval.inputValue() === "manual", "observed Copilot manual mode displayed");
    }
    const runId = (await api("terminal", "attach", { id: s.id })).runId;
    if (!claude) {
      const oldTicket = (await api("chat", "read", { id: s.id })).ticket;
      await page.locator("#session-terminal-view").click();
      await page.evaluate(id => window.workflow.views.get(id).model.click(), s.id);
      const settings = page.locator("#dialog");
      await until(async () => !await settings.getByLabel("Model", { exact: true }).isDisabled(), "Copilot terminal model controls");
      const terminalApproval = settings.getByLabel("Approval level", { exact: true });
      await until(async () => await terminalApproval.inputValue() === "manual", "terminal dialog observes live Copilot permission mode");
      await terminalApproval.selectOption("auto");
      await settings.getByRole("button", { name: "Apply", exact: true }).click();
      await settings.waitFor({ state: "hidden" });
      assert.equal((await api("chat", "read", { id: s.id })).liveApprovalMode, "auto");
      await assert.rejects(api("chat", "approval", { id: s.id, mode: "manual", ...oldTicket }));
      assert.equal((await api("chat", "read", { id: s.id })).liveApprovalMode, "auto");
      assert.equal((await api("terminal", "attach", { id: s.id })).runId, runId);
      await page.locator("#session-chat-view").click();
      await until(async () => await approval.inputValue() === "auto", "Chat shows terminal permission change");
      await approval.selectOption("manual");
      await until(async () => (await api("chat", "read", { id: s.id })).liveApprovalMode === "manual", "Copilot returns to manual approval from Chat");
      assert(await page.locator("#chat-effort").isVisible());
    }
    if (!claude) await approval.selectOption("auto");
    await until(async () => {
      const row = (await api("app", "state")).sessions.find(row => row.id === s.id);
      return row.approvalMode === (claude ? "plan" : "auto");
    }, provider + " approval preference");
    if (!claude) {
      assert.equal((await api("chat", "read", { id: s.id })).liveApprovalMode, "auto");
      await until(async () => await approval.inputValue() === "auto", "observed Copilot assisted mode displayed");
    }
    await page.locator("#chat-input").fill("Check again.");
    await page.locator("#session-terminal-view").click();
    await page.locator("#session-chat-view").click();
    assert.equal(await page.locator("#chat-input").inputValue(), "Check again.");
    await page.locator("#chat-send").click();
    const output = path.join(data, "test-input", `${provider}-chat.bin`);
    await until(() => fs.existsSync(output), provider + " receives chat prompt");
    assert.equal(fs.readFileSync(output, "utf8"), "\x1b[200~Check again.\x1b[201~\r");
    fs.unlinkSync(output);
    assert.equal((await api("terminal", "attach", { id: s.id })).runId, runId);
    await stop();
  }
  console.log("PASS Claude and Copilot Chat retain native conversation, file references, draft and approval settings");
}

async function testSessionNotifications() {
  const profile = path.join(temporary, "notification-profile");
  const folder = path.join(temporary, "notification-project");
  const other = path.join(temporary, "notification-other-project");
  for (const directory of [folder, other]) {
    fs.mkdirSync(directory);
    fs.writeFileSync(path.join(directory, "chat-terminal-fixture"), "");
  }
  await start("", profile);
  const skip = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
  if (await skip.isVisible()) await skip.click();
  const p = await api("app", "add_project", { directory: folder });
  const q = await api("app", "add_project", { directory: other });
  const followed = await api("app", "create_session", { projectId: q.id, provider: "codex", workspaceMode: "direct", tools: false });
  await page.evaluate(async id => { await window.workflow.refresh(); window.workflow.selectSession(id, true); setMode("focus"); }, followed.id);
  const notificationMainHandle = cp.execFileSync("powershell", ["-NoProfile", "-Command",
    '(Get-Process -Id ' + child.pid + ').MainWindowHandle.ToInt64()'], { windowsHide: true, encoding: "utf8" }).trim();
  assert(/^\d+$/.test(notificationMainHandle) && Number(notificationMainHandle) > 0);
  // Focus the synthetic application window, never an existing user terminal.
  const focusWindow = () => cp.execFileSync("powershell", ["-NoProfile", "-Command",
    'Add-Type -TypeDefinition \'using System; using System.Runtime.InteropServices; public static class WorkflowFocusFixture { [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h); [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n); }\'; $w=[IntPtr]' + notificationMainHandle + '; [WorkflowFocusFixture]::ShowWindow($w,9) | Out-Null; [WorkflowFocusFixture]::SetForegroundWindow($w) | Out-Null'
  ], { windowsHide: true, stdio: "pipe" });
  focusWindow();
  await page.locator("#project-title").click();
  await until(() => page.evaluate(() => document.hasFocus()), "native notification test window focused");
  assert.equal((await api("notifications", "read")).enabled, true, "notifications default to enabled");
  const nativePage = await api("browser", "open", {
    sessionId: followed.id, url: "http://127.0.0.1:" + server.address().port, newTab: true,
  });
  await page.evaluate(() => window.workflow.openPanel("browser"));
  await until(async () => (await api("browser", "read", { sessionId: followed.id, pageId: nativePage.id })).elements.some(element => element.text === "Save"), "native child browser active during notification test");

  const cases = [];
  for (const provider of ["codex", "claude", "copilot"]) {
    const s = await api("app", "create_session", { projectId: p.id, provider, workspaceMode: "direct", tools: false });
    const logs = provider === "codex" ? path.join(profile, "test-codex-home/sessions")
      : provider === "claude" ? path.join(profile, "test-claude-home/projects/notice")
      : path.join(profile, "test-copilot-home/session-state", s.id);
    fs.mkdirSync(logs, { recursive: true });
    const log = path.join(logs, provider === "copilot" ? "events.jsonl" : s.id + ".jsonl");
    const append = (...rows) => fs.appendFileSync(log, rows.map(row => JSON.stringify(row) + "\n").join(""));
    const row = (type, payload = {}) => provider === "codex" ? { type: "event_msg", timestamp: new Date().toISOString(), payload: { type, ...payload } }
      : provider === "claude" ? { type, timestamp: new Date().toISOString(), sessionId: s.id, cwd: s.directory, message: { content: payload.text || "" }, ...payload }
      : { type, timestamp: new Date().toISOString(), data: { ...payload, content: payload.text || "" } };
    if (provider === "codex") append({ type: "session_meta", payload: { id: s.id, cwd: s.directory, timestamp: new Date().toISOString() } });
    if (provider === "copilot") append(row("session.start", { sessionId: s.id, context: { cwd: s.directory } }));
    const work = text => provider === "codex" ? append(row("task_started"), row("user_message", { message: text }))
      : append(row(provider === "claude" ? "user" : "user.message", { text }));
    const finish = answer => provider === "codex" ? append(row("task_complete", { last_agent_message: answer }))
      : provider === "claude" ? append(row("assistant", { message: { content: [{ type: "text", text: answer }], stop_reason: "end_turn" } }))
      : append(row("assistant.message", { text: answer }), row("assistant.turn_end"));
    work("An earlier synthetic request."); finish("An earlier result.");
    await api("history", "bind", { id: s.id, conversationId: s.id });
    const run = await api("terminal", "start", { id: s.id });
    await page.evaluate(id => window.workflow.selectSession(id, true), followed.id);
    cases.push({ s, run, work, finish, append, row });
  }
  await sleep(2200);
  assert.deepEqual((await api("notifications", "read")).entries, [], "old history and initial ready prompts never notify");
  const first = cases[0];
  first.work("Perform the synthetic change.");
  await sleep(1100);
  first.finish("The requested change is ready.");
  const entries = async () => (await api("notifications", "read")).entries;
  const notice = await until(async () => (await entries()).find(entry => entry.sessionId === first.s.id), "unattached session completion notification");
  assert.equal(notice.kind, "completed");
  await page.locator("#session-notification").getByRole("button", { name: "View result", exact: true }).waitFor();
  assert(await page.evaluate(() => {
    const card = document.querySelector("#session-notification").getBoundingClientRect();
    const child = document.querySelector("#browser-surface").getBoundingClientRect();
    return card.right <= child.left;
  }), "notification is outside the native child browser and cannot be occluded by it");
  assert.equal(await page.locator('.rail-project[data-project-id="' + p.id + '"] .project-attention').textContent(), "✓");
  const captures = path.join(root, "target/notification-qa");
  fs.mkdirSync(captures, { recursive: true });
  await page.screenshot({ path: path.join(captures, "single-card.png") });
  await sleep(9500);
  assert(await page.locator("#session-notification").isHidden(), "context heartbeat does not restart the completion-card timer");
  assert((await entries()).some(entry => entry.id === notice.id), "transient-card expiry leaves the unseen completion marker");
  await page.locator("#notifications-toggle").click();
  await page.locator("#notification-list .notification-row-open").first().click();
  assert.equal(await page.evaluate(() => focusId), first.s.id);
  assert.equal((await api("terminal", "attach", { id: first.s.id })).runId, first.run.runId, "opening a notification keeps the same run");
  await until(async () => !(await entries()).length, "viewing completion clears its badge");

  first.work("A foreground request."); await sleep(1100); first.finish("The foreground request is ready.");
  await sleep(1800);
  assert(await page.locator("#session-notification").isHidden(), "followed foreground session never shows a card");
  assert(!(await entries()).some(entry => entry.sessionId === first.s.id), "followed result is already seen");
  await page.evaluate(id => window.workflow.selectSession(id, true), followed.id);
  for (const item of cases.slice(1)) {
    const background = item.s.provider === "copilot";
    const nativeBefore = (await api("notifications", "read")).diagnostics;
    assert.equal(nativeBefore.desktopAttempts, 0, "embedded browser does not make a foreground window look like background");
    if (background) {
      cp.execFileSync("powershell", ["-NoProfile", "-Command", 'Add-Type -TypeDefinition \'using System; using System.Runtime.InteropServices; public static class WorkflowMinimizeFixture { [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n); }\'; [WorkflowMinimizeFixture]::ShowWindow([IntPtr]' + notificationMainHandle + ',6) | Out-Null'], { windowsHide: true, stdio: "pipe" });
      await until(async () => !(await api("notifications", "read")).foreground, "minimized native window is not foreground");
    }
    item.work("Check this synthetic change."); await sleep(1100); item.finish("The check passed.");
    await until(async () => (await entries()).some(entry => entry.sessionId === item.s.id && entry.kind === "completed"), item.s.provider + " completion");
    if (background) {
      const native = await until(async () => {
        const snapshot = await api("notifications", "read");
        return snapshot.diagnostics.desktopShown > nativeBefore.desktopShown && snapshot.diagnostics.popupVisible && snapshot.diagnostics.overlayApplied && snapshot;
      }, "visible independent Windows notification card and taskbar overlay");
      assert.equal(native.diagnostics.overlayKind, "completed");
      assert.equal(native.diagnostics.mainMinimized, true);
      assert.equal(native.diagnostics.popupMinimized, false);
      await until(() => page.locator("#session-notification").isHidden(), "background native alert has no duplicate in-app card");
      focusWindow();
      await page.locator("#project-title").click();
    }
  }
  await page.locator("#notifications-toggle").click();
  assert.equal(await page.locator("#notification-list .notification-row").count(), 2);
  await page.keyboard.press("Escape");
  const idsBeforeReload = (await entries()).map(entry => entry.id).sort();
  await page.reload();
  await page.waitForFunction(() => !!window.workflow);
  await sleep(1000);
  assert.deepEqual((await entries()).map(entry => entry.id).sort(), idsBeforeReload, "reattaching UI does not duplicate history");
  assert(await page.locator("#session-notification").isHidden(), "reload does not replay cards");
  await page.locator("#app-settings").click();
  const enabled = page.getByRole("checkbox", { name: "Session notifications", exact: true });
  assert(await enabled.isChecked());
  await enabled.uncheck();
  await until(async () => !(await api("notifications", "read")).enabled, "master notification toggle saves");
  assert.equal(await page.locator(".project-attention").count(), 0);
  assert(await page.locator("#notifications-count").isHidden());
  await page.locator("#dialog-close").click();
  first.work("Muted synthetic request."); await sleep(1100); first.finish("Muted result.");
  await sleep(1300);
  assert(await page.locator("#session-notification").isHidden());
  await page.locator("#app-settings").click();
  await enabled.check();
  await page.locator("#dialog-close").click();
  await sleep(1700);
  assert.deepEqual(await entries(), [], "re-enable does not replay results completed while muted");
  assert(await page.locator("#session-notification").isHidden());
  focusWindow();
  await page.locator("#project-title").click();
  await until(async () => (await api("notifications", "read")).foreground, "main window focused for the in-app question card");
  first.work("Choose the files for this synthetic check.");
  await sleep(1100);
  first.finish("Which files should I check?\n1. One file\n2. All files");
  const question = await until(async () => (await entries()).find(entry => entry.sessionId === first.s.id && entry.kind === "question"), "structured response question is actionable attention");
  await until(async () => (await api("notifications", "read")).diagnostics.overlayKind === "attention", "pending attention has taskbar priority");
  if (!(await page.locator("#session-notification").isVisible())) {
    console.log("In-app question presentation", JSON.stringify(await page.evaluate(() => ({ foreground:notificationState.foreground,
      dialog:document.querySelector('#dialog').open, queued:notificationState.entries.map(e=>({kind:e.kind,presentation:e.presentation,presented:e.presented,silent:e.silent,dismissed:e.dismissed})),
      hasCard:!!notificationCardId }))));
  }
  await page.locator("#session-notification").getByRole("button", { name: "Dismiss notification", exact: true }).click();
  assert((await entries()).some(entry => entry.id === question.id), "closing pending card keeps its marker");
  await page.evaluate(id => window.workflow.selectSession(id, true), first.s.id);
  assert((await entries()).some(entry => entry.id === question.id), "opening a question keeps pending attention");
  first.work("I choose one file.");
  await until(async () => !(await entries()).some(entry => entry.id === question.id), "next user turn resolves the structured question");
  await page.locator("#app-settings").click();
  await enabled.uncheck();
  await page.locator("#dialog-close").click();
  await stop();
  await start("", profile);
  assert.equal((await api("notifications", "read")).enabled, false, "notification preference persists after application restart");
  assert.equal((await api("app", "state")).active.length, 0, "notification support does not restart providers");
  await stop();
  console.log("PASS notification cards, all-provider completions, project badges, focused-session suppression, deduplication, routing and persistent master switch");
}

async function testMinimizedSelectedNotifications() {
  const profile = path.join(temporary, "minimized-selected-profile");
  const folder = path.join(temporary, "minimized-selected-workspace");
  fs.mkdirSync(folder);
  for (const marker of ["chat-terminal-fixture", "notification-terminal-fixture"]) fs.writeFileSync(path.join(folder, marker), "");
  await start("", profile);
  const skip = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
  if (await skip.isVisible()) await skip.click();
  const project = await api("app", "add_project", { directory: folder });
  const s = await api("app", "create_session", { projectId: project.id, provider: "codex", workspaceMode: "direct", tools: false });
  await page.evaluate(async id => { await window.workflow.refresh(); window.workflow.selectSession(id, true); setMode("focus"); }, s.id);
  const run = await api("terminal", "start", { id: s.id });
  await page.evaluate(() => window.workflow.refresh());
  const nativeWindow = (operation, handle = 0) => cp.execFileSync("powershell", ["-NoProfile", "-Command",
    'Add-Type -TypeDefinition \'using System; using System.Runtime.InteropServices; public static class NotificationWindowFixture { [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int n); [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h); [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h,uint m,IntPtr w,IntPtr l); }\'; '
      + (operation === "handle" ? '(Get-Process -Id ' + child.pid + ').MainWindowHandle.ToInt64()'
        : operation === "foreground" ? '[NotificationWindowFixture]::GetForegroundWindow().ToInt64()'
        : operation === "minimize" ? '[NotificationWindowFixture]::ShowWindow([IntPtr]' + handle + ',6) | Out-Null'
        : operation === "close" ? '[NotificationWindowFixture]::PostMessageW([IntPtr]' + handle + ',16,[IntPtr]::Zero,[IntPtr]::Zero) | Out-Null'
        : '[NotificationWindowFixture]::ShowWindow([IntPtr]' + handle + ',9) | Out-Null; [NotificationWindowFixture]::SetForegroundWindow([IntPtr]' + handle + ') | Out-Null')
  ], { windowsHide: true, encoding: "utf8", stdio: "pipe" }).trim();
  const mainHandle = nativeWindow("handle");
  assert(Number(mainHandle) > 0);
  nativeWindow("restore", mainHandle);
  await page.locator("#project-title").click();
  await until(() => page.evaluate(() => notificationFollowing() === focusId), "selected live session is being followed");
  assert((await api("app", "state")).sessions.find(item => item.id === s.id).conversationId == null);
  const popupPage = await until(() => browser.contexts().flatMap(c => c.pages()).find(p => /\/notification\.html$/.test(p.url())), "isolated popup WebView");
  for (const [domain, action] of [["app", "state"], ["terminal", "start"], ["files", "list"], ["teams", "send"]]) {
    await assert.rejects(popupPage.evaluate(({ domain, action }) => window.__TAURI__.core.invoke("dispatch", { domain, action, args: {} }), { domain, action }), /erişemez/);
  }
  const externalBrowser = await playwright.chromium.launch({ channel: "msedge", headless: false, args: ["--no-first-run", "--no-default-browser-check"] });
  try {
    const external = await externalBrowser.newPage();
    await external.setContent("<!doctype html><title>Workflow notification focus fixture</title><h1>Notification focus fixture</h1>");
    const send = text => api("terminal", "write", { id: s.id, runId: run.runId, data: text + "\r" });
    const entries = async () => (await api("notifications", "read")).entries;
    const waitPopup = async kind => {
      const state = await until(async () => {
        const state = await api("notifications", "read");
        return state.diagnostics.popupVisible && !state.diagnostics.popupMinimized && state.entries.some(entry => entry.sessionId === s.id && entry.kind === kind && entry.presented) && state;
      }, "visible desktop " + kind + " card while main remains minimized");
      assert.equal(state.diagnostics.mainMinimized, true);
      await popupPage.locator('#card[data-kind="' + kind + '"]').waitFor();
      const content = await popupPage.locator("body").textContent();
      assert(!content.includes(folder) && !content.includes(s.id) && !content.includes(project.name), "desktop card contains no session paths or project identity");
      return state.entries.find(entry => entry.sessionId === s.id && entry.kind === kind);
    };
    nativeWindow("restore", mainHandle);
    await page.locator("#project-title").click();
    await page.evaluate(() => updateNotificationContext(true));
    const stale = await page.evaluate(() => ({ sessionId: focusId, focused: true, contextToken: notificationContextToken, contextRevision: ++notificationContextRevision, language: uiLanguage }));
    await send("short");
    nativeWindow("minimize", mainHandle);
    await external.bringToFront();
    const externalFocus = nativeWindow("foreground");
    assert.notEqual(externalFocus, mainHandle);
    const completed = await waitPopup("completed");
    assert.equal(nativeWindow("foreground"), externalFocus, "desktop card never steals foreground focus");
    await api("notifications", "context", stale);
    await api("notifications", "seen", { id: completed.id });
    assert((await entries()).some(entry => entry.id === completed.id), "stale foreground context or seen cannot consume a minimized selected session result");
    const captures = path.join(root, "target/notification-qa");
    fs.mkdirSync(captures, { recursive: true });
    await popupPage.screenshot({ path: path.join(captures, "desktop-completed.png") });
    await popupPage.locator("#open").click();
    await until(async () => !(await api("notifications", "read")).diagnostics.mainMinimized, "popup opens the existing main window");
    await until(() => page.evaluate(id => focusId === id, s.id), "popup selects the same existing session");
    assert.equal((await api("terminal", "attach", { id: s.id })).runId, run.runId);
    assert(!(await entries()).some(entry => entry.id === completed.id));

    for (const kind of ["approval", "question"]) {
      await send(kind);
      nativeWindow("minimize", mainHandle);
      await external.bringToFront();
      const foregroundBefore = nativeWindow("foreground");
      const pending = await waitPopup(kind);
      assert.equal(nativeWindow("foreground"), foregroundBefore);
      await popupPage.screenshot({ path: path.join(captures, "desktop-" + kind + ".png") });
      if (kind === "approval") {
        console.log("PASS independent approval card visibly displayed without restoring main or taking focus");
        await sleep(process.env.WORKFLOW_TEST_NOTIFICATION_CAPTURE === "1" ? 30000 : 10500);
        assert((await api("notifications", "read")).diagnostics.popupVisible, "approval card does not expire like a completed card");
        await popupPage.locator("#dismiss").click();
        assert((await entries()).some(entry => entry.id === pending.id), "desktop dismiss preserves approval marker");
        await send("answer");
        await waitPopup("completed");
        await popupPage.locator("#open").click();
        await until(async () => !(await api("notifications", "read")).diagnostics.mainMinimized, "restore after approval answer");
      } else {
        nativeWindow("close", mainHandle);
        await until(() => child.exitCode !== null, "closing main also destroys the independent question card", 10000);
        let alive = true;
        try { process.kill(run.pid, 0); } catch { alive = false; }
        assert.equal(alive, false, "main close stops its provider even while popup is visible");
      }
    }
  } finally { await externalBrowser.close(); await stop(); }
  console.log("PASS actual selected-session minimize order, short Codex reply without duration footer, stale acknowledgement rejection, visible completion/approval/question cards, exact routing and main-close lifecycle");
}

async function testPendingChatInteraction() {
  for (const [provider, replyMode] of [["codex", "choice"], ["claude", "choice"], ["copilot", "choice"], ["codex", "custom"]]) {
    const profile = path.join(temporary, provider + "-" + replyMode + "-interaction-profile");
    const folder = path.join(temporary, provider + "-" + replyMode + "-interaction-workspace");
    for (const name of [`${provider}-interaction.bin`, `${provider}-custom.txt`]) {
      fs.rmSync(path.join(data, "test-input", name), { force: true });
    }
    fs.mkdirSync(folder);
    for (const name of ["chat-terminal-fixture", "chat-interaction-fixture"]) {
      fs.writeFileSync(path.join(folder, name), "");
    }
    await start("", profile);
    assert(await page.evaluate(() => {
      const valid = chatReplyQuestion("  - Existing detail\n\nWhich change should I make?\n1. **One**\n   Keep other files.\n2. Everything?");
      if (!valid || valid.before !== "  - Existing detail" || valid.choices[1].label !== "Everything?") return false;
      if (questionOption("One — first — second").description !== "first — second") return false;
      return ["Why did it fail?\n1. A timeout\n2. Invalid input", "Which change?\n1. One\n2. Both\n\nUnrelated paragraph", "Which change?\n1.   \n2. Both"]
        .every(text => chatReplyQuestion(text) === null);
    }), "question parser preserves content and rejects ordinary explanations");
    const p = await api("app", "add_project", { directory: folder });
    const s = await api("app", "create_session", { projectId: p.id, provider, tools: false });
    if (provider === "codex") {
      const logs = path.join(profile, "test-codex-home", "sessions");
      fs.mkdirSync(logs, { recursive: true });
      fs.writeFileSync(path.join(logs, "rollout-choice-test.jsonl"), [
        { type: "session_meta", payload: { id: "chat-choice-test", cwd: s.directory, timestamp: new Date().toISOString() } },
        { type: "event_msg", payload: { type: "task_started" } },
        { type: "event_msg", payload: { type: "user_message", message: "What can I change?" } },
        { type: "event_msg", payload: { type: "task_complete", last_agent_message: "Which changes should I make?\n\n1 - **Only one**\n   Leave the remaining files unchanged.\n2 - **All of them**\n   Apply the change to every file." } },
      ].map(entry => JSON.stringify(entry)).join("\n") + "\n");
      await api("history", "bind", { id: s.id, conversationId: "chat-choice-test" });
    }
    const skip = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
    if (await skip.isVisible()) await skip.click();
    await page.evaluate(async id => {
      await window.workflow.refresh();
      window.workflow.selectSession(id, true);
    }, s.id);
    await page.locator("#session-chat-view").click();
    await page.locator("#chat-start").click();
    await until(async () => !!(await api("chat", "read", { id: s.id })).ticket, provider + " composer ready");
    const runId = (await api("terminal", "attach", { id: s.id })).runId;
    const input = page.locator("#chat-input");
    const send = page.locator("#chat-send");
    const card = page.locator("#chat-terminal-prompt");
    assert(await page.evaluate(async () => {
      const view = {};
      let result = false, attempts = 0;
      const demo = createChatQuestion(view, "local-retry-test", "Which file?", [{ label: "One" }, { label: "All" }], async () => { attempts++; return result; });
      document.body.append(demo);
      updateQuestionControls(demo, false);
      const field = demo.querySelector(".chat-custom-input");
      field.value = "Keep my typed response";
      field.dispatchEvent(new Event("input"));
      demo.querySelector(".chat-custom-send").click();
      await new Promise(resolve => setTimeout(resolve, 0));
      const retained = view.questionReply?.text === field.value;
      updateQuestionControls(demo, false);
      result = true;
      demo.querySelector(".chat-custom-send").click();
      await new Promise(resolve => setTimeout(resolve, 0));
      const done = retained && attempts === 2 && view.questionReply === null;
      demo.remove();
      return done;
    }), "unsubmitted custom answer is retained for retry");
    if (provider === "codex") {
      await input.fill("Keep my original draft.");
      const plainChoice = page.locator(".chat-reply-question").getByRole("radio", { name: /All of them/ });
      await plainChoice.waitFor();
      assert(await page.evaluate(id => {
        const view = chatView(id);
        view.loadError = true;
        paintChatComposer(session(id), view);
        const controls = [...document.querySelectorAll(".chat-reply-question input, .chat-reply-question button")];
        const disabled = controls.length > 0 && controls.every(control => control.disabled);
        return disabled;
      }, s.id));
      const reply = replyMode === "custom" ? "Use the shared validator." : "2";
      if (replyMode === "custom") {
        const plainCard = page.locator(".chat-reply-question");
        await plainCard.getByRole("textbox", { name: "Your own answer", exact: true }).fill(reply);
        const captures = path.join(root, "target", "question-card-qa");
        fs.mkdirSync(captures, { recursive: true });
        await plainCard.screenshot({ path: path.join(captures, "chat-question.png") });
        await plainCard.getByRole("button", { name: "Send answer", exact: true }).click();
      } else {
        await plainChoice.click();
        await page.locator(".chat-reply-question").getByRole("button", { name: "Send selection", exact: true }).click();
      }
      const submitted = path.join(data, "test-input", "codex-interaction.bin");
      await until(() => fs.existsSync(submitted), "clicking the plain chat option sends the chosen number");
      assert.equal(fs.readFileSync(submitted, "utf8"), "\x1b[200~" + reply + "\x1b[201~\r");
      assert.equal(await input.inputValue(), "Keep my original draft.");
    } else {
      await input.fill("Prepare the sample output.");
      await send.click();
    }
    await card.getByText("Approval required", { exact: true }).waitFor();
    const attentionEntries = async () => (await api("notifications", "read")).entries.filter(entry => entry.sessionId === s.id);
    const approvalNotice = await until(async () => (await attentionEntries()).find(entry => entry.kind === "approval"), provider + " approval notification independent of terminal rendering");
    await api("notifications", "dismiss", { id: approvalNotice.id });
    assert((await attentionEntries()).some(entry => entry.id === approvalNotice.id), "dismissing the card does not resolve approval");
    await api("notifications", "seen", { id: approvalNotice.id });
    assert((await attentionEntries()).some(entry => entry.id === approvalNotice.id), "opening the session does not resolve approval");
    assert.match(await card.locator("pre").textContent(), /Read the sample notes/);
    assert.match(await card.locator("pre").textContent(), /2\. No/);
    const live = await api("chat", "read", { id: s.id });
    assert.equal(live.ticket, null);
    assert.equal(!!live.terminalPrompt.menu, provider === "codex");
    if (live.terminalPrompt.menu) await assert.rejects(api("chat", "answer_prompt", { id: s.id, runId, ...live.terminalPrompt.menu, text: "Skip this operation" }), /özel yanıtı/);
    await input.fill("Keep this separate draft.");
    assert(await send.isDisabled());
    await card.getByRole("button", { name: "Respond in Terminal" }).click();
    assert(await page.locator("#chat-view").isHidden());
    assert.equal((await api("terminal", "attach", { id: s.id })).runId, runId);
    await page.locator("#session-chat-view").click();
    await card.getByText("Approval required", { exact: true }).waitFor();
    assert.equal(await input.inputValue(), "Keep this separate draft.");

    const answerFixture = () => api("terminal", "write", { id: s.id, runId, data: "1\r" });
    await page.waitForFunction(id => !chatView(id).reading, s.id);
    await page.evaluate(id => {
      const chat = chatView(id);
      clearTimeout(chatPollTimer);
      chatPollTimer = undefined;
      // Simulate transcript IO still in flight while the native renderer is detached.
      chat.reading = true;
      chat.terminalPrompt = null;
      chat.ticket = null;
      const terminal = views.get(id);
      window.interactionRendererState = { runId: terminal.runId, ready: terminal.ready, replaying: terminal.replaying };
      terminal.runId = null;
      terminal.ready = false;
      terminal.replaying = true;
      paintChatComposer(session(id), chat);
    }, s.id);
    await answerFixture();
    await card.getByRole("radiogroup", { name: "Apply the change where?" }).waitFor();
    assert(await page.evaluate(id => chatView(id).reading, s.id), "live question appears while transcript loading remains pending");
    const questionNotice = await until(async () => (await attentionEntries()).find(entry => entry.kind === "question"), provider + " question replaces answered approval");
    assert(!(await attentionEntries()).some(entry => entry.id === approvalNotice.id), "answered approval clears its marker");
    await sleep(1000);
    assert.equal((await attentionEntries()).find(entry => entry.kind === "question")?.id, questionNotice.id, "question redraw does not duplicate notifications");
    const choiceMenu = async () => (await api("chat", "read", { id: s.id })).terminalPrompt?.menu;
    const first = await choiceMenu();
    assert.deepEqual(first.choices, ["Only one", "All of them"]);
    assert.deepEqual(first.descriptions, ["Leave the remaining files unchanged.", "Apply the change to every file."]);
    await card.getByText("Apply the change to every file.", { exact: true }).waitFor();
    assert(await send.isDisabled());
    const answerPrompt = (menu, choice, expectedRun = runId) =>
      api("chat", "answer_prompt", { id: s.id, ...menu, runId: expectedRun, choice });
    await assert.rejects(answerPrompt(first, 2, "stale"), /çalıştırması değişti/);
    await assert.rejects(answerPrompt(first, 8), /Soru değişti/);
    const radio = card.getByRole("radio", { name: /Only one/ });
    await radio.focus();
    await radio.press("ArrowDown");
    assert.equal(await card.getByRole("radio", { name: /All of them/ }).isChecked(), true);
    assert.equal((await choiceMenu()).key, first.key, "arrow navigation does not send a reply");
    await card.getByRole("button", { name: "Send selection", exact: true }).click();
    await card.getByRole("radiogroup", { name: "Apply the test where?" }).waitFor();
    await page.evaluate(id => {
      Object.assign(views.get(id), window.interactionRendererState);
      delete window.interactionRendererState;
      chatView(id).reading = false;
      scheduleChatRead();
    }, s.id);
    const second = await choiceMenu();
    assert.notEqual(first.key, second.key);
    await assert.rejects(answerPrompt({ ...second, key: first.key }, 2), /Soru değişti/);
    await assert.rejects(answerPrompt(first, 2), /Soru değişti/);
    await card.getByRole("radio", { name: /Only one/ }).click();
    await card.getByRole("button", { name: "Send selection", exact: true }).click();
    {
      await card.getByRole("radiogroup", { name: "What should I do instead?" }).waitFor();
      const customMenu = await choiceMenu();
      assert.equal(customMenu.customChoice, 3);
      assert.equal(await card.getByRole("radio").count(), 2);
      const customInput = card.getByRole("textbox", { name: "Your own answer", exact: true });
      const customAnswer = "Yalnızca yapılandırma dosyasını güncelle.";
      await customInput.fill(customAnswer);
      await page.setViewportSize({ width: 1100, height: 620 });
      await card.getByRole("textbox", { name: "Your own answer", exact: true }).scrollIntoViewIfNeeded();
      assert(await card.getByRole("textbox", { name: "Your own answer", exact: true }).evaluate(field => {
        const rect = field.getBoundingClientRect(), viewport = document.querySelector("#viewport").getBoundingClientRect();
        return rect.top >= viewport.top && rect.bottom <= viewport.bottom;
      }), provider + " question controls remain reachable in a short window");
      const metrics = await page.context().newCDPSession(page);
      await metrics.send("Emulation.clearDeviceMetricsOverride");
      await metrics.detach();
      const captures = path.join(root, "target", "question-card-qa");
      fs.mkdirSync(captures, { recursive: true });
      await card.screenshot({ path: path.join(captures, provider + "-native-question.png") });
      await assert.rejects(api("chat", "answer_prompt", { id: s.id, runId, ...customMenu, text: "invalid\nreply" }), /Özel yanıt/);
      await assert.rejects(api("chat", "answer_prompt", { id: s.id, runId, ...customMenu, key: first.key, text: customAnswer }), /Soru değişti/);
      await customInput.press("Enter");
      await until(async () => !(await choiceMenu()), "answered custom menu stays consumed before provider redraw");
      const echoed = await api("chat", "read", { id: s.id });
      assert.equal(echoed.terminalPrompt?.menu, undefined);
      const received = path.join(data, "test-input", provider + "-custom.txt");
      await until(() => fs.existsSync(received), provider + " native question receives the custom answer");
      assert.equal(fs.readFileSync(received, "utf8"), customAnswer);
      await assert.rejects(api("chat", "answer_prompt", { id: s.id, runId, ...customMenu, text: customAnswer }), /Soru değişti/);
    }
    await card.waitFor({ state: "hidden" });
    assert(await page.locator("#chat-view").isVisible());
    assert.equal((await api("terminal", "attach", { id: s.id })).runId, runId);
    assert.equal(fs.readFileSync(path.join(data, "test-input", `${provider}-choice-0.txt`), "utf8"), "2");
    assert.equal(fs.readFileSync(path.join(data, "test-input", `${provider}-choice-1.txt`), "utf8"), "1");
    await page.locator("#chat-terminal-output").getByText("Fixture interaction uses a different layout.").waitFor();
    await page.getByRole("button", { name: "Hide live Terminal output" }).click();
    assert(await page.locator("#chat-terminal-output").isHidden());
    await page.getByRole("button", { name: "Show live Terminal output" }).click();
    await answerFixture();
    await page.locator("#chat-terminal-output").waitFor({ state: "hidden" });
    assert.equal(await input.inputValue(), "Keep this separate draft.");
    assert(await card.isHidden());
    await stop();
  }
  console.log("PASS clickable Chat choices for text and native menus, stale choice rejection, approval display and fallback for all three providers");
}

async function testArchiveStorage() {
  const profile = path.join(temporary, "archive-storage-profile");
  const source = path.join(temporary, "archive-source");
  const repository = path.join(temporary, "archive-repository");
  const nested = path.join(repository, "component");
  fs.mkdirSync(source);
  fs.mkdirSync(nested, { recursive: true });
  fs.writeFileSync(path.join(source, "source.txt"), "original workspace\n");
  fs.writeFileSync(path.join(nested, "tracked.txt"), "initial state\n");
  const git = (directory, ...args) => cp.execFileSync("git", args, {
    cwd: directory, encoding: "utf8", windowsHide: true,
  }).trim();
  git(repository, "init");
  git(repository, "add", ".");
  git(repository, "-c", "user.name=Workflow Test", "-c", "user.email=test@localhost",
    "commit", "-m", "Initial archive test");

  await start("", profile);
  const skip = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
  if (await skip.isVisible()) await skip.click();
  const plain = await api("app", "add_project", { directory: source });
  const project = await api("app", "add_project", { directory: nested });
  const makeSession = (projectId, workspaceMode) =>
    api("app", "create_session", { projectId, provider: "codex", workspaceMode, tools: false });
  const copy = await makeSession(plain.id, "isolated");
  const worktree = await makeSession(project.id, "isolated");
  const direct = await makeSession(plain.id, "direct");
  assert.equal(copy.workspaceKind, "copy");
  assert.equal(worktree.workspaceKind, "worktree");
  const slot = (s, archived) => path.join(profile, archived ? "archive" : "workspaces", s.id);
  const change = (s, archived) => api("app", "session_patch", { id: s.id, archived });
  const assertLocations = (s, archived) => {
    assert(fs.existsSync(path.join(slot(s, archived), "baseline.json")));
    assert(!fs.existsSync(slot(s, !archived)), "only one managed folder may exist");
  };
  const copyArchive = slot(copy, true);
  fs.mkdirSync(copyArchive, { recursive: true });
  await assert.rejects(change(copy, true), /oturum klasörü zaten var/);
  assert(fs.existsSync(copy.directory));
  assert(fs.existsSync(copyArchive));
  fs.rmdirSync(copyArchive);
  fs.writeFileSync(path.join(copy.directory, "copy-only.txt"), "preserved file\n");
  fs.writeFileSync(path.join(worktree.directory, "tracked.txt"), "staged edit\n");
  git(worktree.directory, "add", "tracked.txt");
  fs.writeFileSync(path.join(worktree.directory, "tracked.txt"), "unstaged edit\n");
  fs.writeFileSync(path.join(worktree.directory, "untracked.txt"), "preserved too\n");
  const originalStatus = git(worktree.directory, "status", "--porcelain");
  const originalAdmin = git(worktree.directory, "rev-parse", "--absolute-git-dir");
  const originalBranch = git(worktree.directory, "branch", "--show-current");
  const root = git(worktree.directory, "rev-parse", "--show-toplevel");
  git(repository, "worktree", "lock", root);
  await assert.rejects(change(worktree, true), /worktree kilitli/);
  assertLocations(worktree, false);
  git(repository, "worktree", "unlock", root);
  const logs = path.join(profile, "test-codex-home", "sessions");
  fs.mkdirSync(logs, { recursive: true });
  fs.writeFileSync(path.join(logs, "rollout-archive-fixture.jsonl"), [
    { type: "session_meta", payload: { id: "archive-fixture", cwd: worktree.directory, timestamp: new Date().toISOString() } },
    { type: "response_item", payload: { role: "assistant", content: [{ text: "Archive history fixture" }] } },
  ].map(JSON.stringify).join("\n") + "\n");
  await api("history", "bind", { id: worktree.id, conversationId: "archive-fixture" });

  const archivedDirect = await change(direct, true);
  assert.equal(archivedDirect.directory, direct.directory);
  assert.equal(fs.readFileSync(path.join(source, "source.txt"), "utf8"), "original workspace\n");
  const archivedCopy = await change(copy, true);
  assertLocations(copy, true);
  assert.equal(fs.readFileSync(path.join(archivedCopy.directory, "copy-only.txt"), "utf8"), "preserved file\n");
  const archivedTree = await change(worktree, true);
  for (const session of [direct, copy, worktree]) {
    await assert.rejects(api("terminal", "start", { id: session.id }), /Arşivlenmiş|Restore the archived/);
    assert.equal((await api("app", "state")).active.some(run => run.id === session.id), false);
  }
  assertLocations(worktree, true);
  assert.equal(archivedTree.archivedFromDirectory, worktree.directory);
  assert.equal(fs.realpathSync(archivedTree.directory), fs.realpathSync(path.join(slot(worktree, true), "files", "component")));
  assert.equal(git(archivedTree.directory, "status", "--porcelain"), originalStatus);
  assert.equal(git(archivedTree.directory, "rev-parse", "--absolute-git-dir"), originalAdmin);
  assert.equal(git(archivedTree.directory, "branch", "--show-current"), originalBranch);
  assert.match((await api("history", "messages", { id: worktree.id })).messages[0].text, /Archive history fixture/);

  assert.equal((await change(copy, false)).directory, copy.directory);
  assertLocations(copy, false);
  assert.equal((await change(worktree, false)).directory, worktree.directory);
  assertLocations(worktree, false);
  assert.equal(git(worktree.directory, "status", "--porcelain"), originalStatus);
  const additional = await makeSession(plain.id, "isolated");
  const activeRun = await api("terminal", "start", { id: additional.id });
  pids.push(activeRun.pid);
  const dialog = page.locator("#dialog");
  const sidebar = page.locator("#project-sessions");
  const menu = page.locator("#workspace-action-menu");
  const archiveFolder = async (folderId) => {
    await sidebar.locator('[data-folder-menu="' + folderId + '"]').click();
    await menu.getByRole("menuitem", { name: "Move folder to archive", exact: true }).click();
  };
  const restoreFolder = async () => {
    await page.locator("#archive").click();
    const group = page.locator(".archive-folder-group").filter({
      has: page.locator('.archive-row[data-session-id="' + copy.id + '"]'),
    });
    await group.getByRole("button", { name: "Restore folder", exact: true }).click();
    await dialog.getByRole("button", { name: "Restore folder", exact: true }).click();
    await dialog.waitFor({ state: "hidden" });
  };
  const selectCopy = () => page.evaluate(async id => {
    await window.workflow.refresh();
    window.workflow.selectSession(id);
  }, copy.id);
  await selectCopy();
  const generalArchive = sidebar.getByRole("button", { name: "Move folder to archive", exact: true });
  await generalArchive.click();
  await dialog.getByRole("button", { name: "Move folder to archive", exact: true }).click();
  await dialog.waitFor({ state: "hidden" });
  assert((await api("terminal", "attach", { id: additional.id })).exited);
  await sidebar.locator('[data-folder-id=""]').waitFor({ state: "hidden" });
  assertLocations(copy, true);
  assertLocations(additional, true);
  await restoreFolder();
  await sidebar.locator('[data-folder-id=""]').waitFor();
  assertLocations(copy, false);
  assertLocations(additional, false);
  await generalArchive.click();
  await sidebar.locator('[data-folder-id=""]').waitFor({ state: "hidden" });
  assert(await dialog.isHidden(), "a closed General folder archives directly");
  assertLocations(copy, true);
  assertLocations(additional, true);
  await restoreFolder();

  const topic = await api("app", "folder_create", { projectId: plain.id, name: "Archive test topic" });
  for (const s of [copy, additional]) {
    await api("app", "folder_move_session", { projectId: plain.id, folderId: topic.id, id: s.id });
  }
  fs.mkdirSync(slot(additional, true), { recursive: true });
  await selectCopy();
  const folderTrigger = sidebar.locator('[data-folder-menu="' + topic.id + '"]');
  await folderTrigger.click();
  assert.equal(await folderTrigger.getAttribute("aria-expanded"), "true");
  await page.keyboard.press("End");
  assert.equal(await page.locator(":focus").textContent(), "Remove folder");
  await page.keyboard.press("Escape");
  await menu.waitFor({ state: "hidden" });
  assert(await dialog.isHidden());
  await archiveFolder(topic.id);
  await page.locator("#toast span").filter({ hasText: /following sessions could not be moved/i }).waitFor();
  assertLocations(copy, true);
  assert(fs.existsSync(additional.directory));
  fs.rmdirSync(slot(additional, true));
  await archiveFolder(topic.id);
  await sidebar.locator('[data-folder-id="' + topic.id + '"]').waitFor({ state: "hidden" });
  await restoreFolder();
  await sidebar.locator('[data-folder-id="' + topic.id + '"]').waitFor();
  assertLocations(copy, false);
  assertLocations(additional, false);
  assert((await api("app", "state")).sessions.filter(s => [copy.id, additional.id].includes(s.id))
    .every(s => s.folderId === topic.id && !s.archived));
  await stop();

  // Simulate an interrupted move before the workspace database was updated.
  const journal = path.join(profile, "archive-transactions", worktree.id);
  fs.writeFileSync(journal, JSON.stringify(fs.realpathSync(originalAdmin)));
  const canonicalProfile = fs.realpathSync(profile);
  assert.equal(path.dirname(fs.realpathSync(path.dirname(slot(worktree, false)))), canonicalProfile);
  assert.equal(path.dirname(fs.realpathSync(path.dirname(slot(worktree, true)))), canonicalProfile);
  fs.renameSync(slot(worktree, false), slot(worktree, true));
  const db = path.join(profile, "workspace.json");
  const previous = JSON.parse(fs.readFileSync(db, "utf8"));
  previous.sessions.find(s => s.id === copy.id).archived = true;
  fs.writeFileSync(db, JSON.stringify(previous));
  await start("", profile);
  assertLocations(worktree, false);
  assert.equal(fs.existsSync(journal), false);
  assert.equal(git(worktree.directory, "status", "--porcelain"), originalStatus);
  await page.locator("#archive").click();
  const legacy = page.locator('#session-history .archive-row[data-session-id="' + copy.id + '"]');
  await legacy.getByRole("button", { name: "Move folder to archive" }).click();
  await until(() => fs.existsSync(slot(copy, true)), "legacy archive folder moved");
  await legacy.getByRole("button", { name: "Move folder to archive" }).waitFor({ state: "hidden" });
  assertLocations(copy, true);
  await stop();
  console.log("PASS physical workspace archive and recovery, General and named folder archive and restore, and retry after a partial move");
}

async function testTargetBranchMerge() {
  const base = path.join(temporary, "target-merge");
  const folder = path.join(base, "project");
  const profile = path.join(base, "profile");
  fs.mkdirSync(folder, { recursive: true });
  const gitAt = (cwd, ...args) => cp.execFileSync("git", args, { cwd, encoding: "utf8", windowsHide: true });
  const commit = (cwd, file, text) => {
    fs.writeFileSync(path.join(cwd, file), text);
    gitAt(cwd, "add", "--", file);
    gitAt(cwd, "commit", "-m", "Synthetic target merge check");
  };
  gitAt(folder, "init", "-b", "main");
  gitAt(folder, "config", "user.name", "Workflow Test");
  gitAt(folder, "config", "user.email", "test@localhost");
  gitAt(folder, "config", "commit.gpgsign", "false");
  gitAt(folder, "config", "core.autocrlf", "false");
  const hooks = path.join(base, "hooks");fs.mkdirSync(hooks);
  gitAt(folder, "config", "core.hooksPath", hooks);
  commit(folder, "feature.txt", "base\n");
  const baseline = gitAt(folder, "rev-parse", "HEAD").trim();
  await start("", profile);
  // Late results from a dismissed dialog must never close or alter its replacement.
  for (const rejected of [false, true]) {
    await page.evaluate(() => {
      const message = document.createElement("p");message.textContent = "Synthetic pending action";
      window.modal("Pending dialog", message, () => new Promise((resolve, reject) => {
        window.finishDialogFixture = { resolve, reject };
      }), "Apply fixture");
    });
    await page.locator("#dialog").getByRole("button", { name: "Apply fixture", exact: true }).click();
    await page.waitForFunction(() => !!window.finishDialogFixture);
    await page.locator("#dialog-footer").getByRole("button", { name: "Cancel", exact: true }).click();
    await page.locator("#app-settings").click();
    await page.locator("#dialog").getByRole("heading", { name: "Settings", exact: true }).waitFor();
    await page.evaluate(rejected => {
      if (rejected) window.finishDialogFixture.reject(new Error("Synthetic delayed failure"));
      else window.finishDialogFixture.resolve();
      delete window.finishDialogFixture;
    }, rejected);
    await page.waitForTimeout(150);
    assert(await page.locator("#dialog").getByRole("heading", { name: "Settings", exact: true }).isVisible());
    assert.equal(await page.locator("#dialog .dialog-error").count(), 0);
    await page.locator("#dialog-footer").getByRole("button", { name: "Close", exact: true }).click();
  }
  console.log("PASS dismissed modal success/failure leaves a replacement modal intact");
  const project = await api("app", "add_project", { directory: folder });
  const source = await api("app", "create_session", { projectId: project.id, provider: "codex", name: "Merge source" });
  const context = { projectId: project.id, sessionId: source.id };
  const dialog = page.locator("#dialog");
  await page.evaluate(async ({ project, source }) => {
    await window.sync();window.workflow.selectProject(project);window.workflow.selectSession(source, false, "changes");
  }, { project: project.id, source: source.id });
  await page.locator("#changes-mode-git").click();
  const open = async (target) => {
    await page.locator("#git-more").click();
    await dialog.getByRole("button", { name: "Merge into target branch", exact: true }).click();
    if (target) await dialog.getByLabel("Target branch", { exact: true }).selectOption(target);
  };
  const ready = async () => {
    const button = dialog.locator("#dialog-footer").getByRole("button", { name: "Merge", exact: true });
    await until(async () => await button.isEnabled(), "merge preview ready");return button;
  };
  const sourceBranch = gitAt(source.directory, "branch", "--show-current").trim();
  commit(source.directory, "feature.txt", "source change\n");
  await page.evaluate(() => window.refreshPanel());
  await open();
  assert.equal(await dialog.getByLabel("Target branch", { exact: true }).inputValue(), "main");
  await dialog.getByText(sourceBranch + " → main", { exact: true }).waitFor();
  await (await ready()).click();await dialog.waitFor({ state: "hidden" });
  assert.equal(fs.readFileSync(path.join(folder, "feature.txt"), "utf8"), "source change\n");
  assert.equal(gitAt(source.directory, "branch", "--show-current").trim(), sourceBranch);
  assert.equal((await api("app", "state")).sessions.length, 1);
  gitAt(folder, "branch", "release/test", baseline);
  await page.evaluate(() => window.refreshPanel());await open("release/test");
  await (await ready()).click();await dialog.waitFor({ state: "hidden" });
  assert.equal(gitAt(folder, "rev-parse", "release/test").trim(), gitAt(source.directory, "rev-parse", "HEAD").trim());
  fs.writeFileSync(path.join(folder, "uncommitted.txt"), "preserved draft");
  await open("main");await dialog.getByText(/The target branch has uncommitted changes/).waitFor();
  assert(await dialog.locator("#dialog-footer .primary").isDisabled());
  await dialog.locator("#dialog-footer button").first().click();fs.unlinkSync(path.join(folder, "uncommitted.txt"));
  const direct = await api("app", "create_session", { projectId: project.id, provider: "codex", workspaceMode: "direct", name: "Existing target session" });
  const run = await api("terminal", "start", { id: direct.id });
  await open("main");await dialog.getByText("Stop sessions using the target branch first.", { exact: true }).waitFor();
  assert(await dialog.locator("#dialog-footer .primary").isDisabled());
  await dialog.locator("#dialog-footer button").first().click();
  await api("terminal", "stop", { id: direct.id, runId: run.runId });
  await until(async () => !(await api("app", "state")).active.some(item => item.id === direct.id), "target session stopped");
  commit(folder, "feature.txt", "target change\n");commit(source.directory, "feature.txt", "conflicting source change\n");
  const targetBefore = gitAt(folder, "rev-parse", "HEAD").trim();
  await open("main");await (await ready()).click();
  await dialog.getByText("Conflicts found; the target branch was not changed. You can resolve them in this session.", { exact: true }).waitFor();
  assert.equal(gitAt(folder, "rev-parse", "HEAD").trim(), targetBefore);
  assert.equal(gitAt(folder, "status", "--porcelain").trim(), "");
  await dialog.getByRole("button", { name: "Resolve conflicts in this session", exact: true }).click();
  await until(async () => (await api("git", "state", context)).files.some(file => file.conflict), "source conflict review");
  let state = await api("git", "state", context);
  const details = await api("git", "conflict_details", { ...context, path: "feature.txt" });
  await api("git", "conflict_resolve", { ...context, stamp: state.stamp, fileStamp: details.hash, path: "feature.txt", choice: "ours" });
  state = await api("git", "state", context);
  await api("git", "merge_continue", { ...context, stamp: state.stamp });
  await page.evaluate(() => window.refreshPanel());await open("main");
  await (await ready()).click();await dialog.waitFor({ state: "hidden" });
  assert.equal(gitAt(folder, "rev-parse", "HEAD").trim(), gitAt(source.directory, "rev-parse", "HEAD").trim());
  assert.equal(gitAt(source.directory, "branch", "--show-current").trim(), sourceBranch);
  await open("main");
  gitAt(source.directory, "switch", "-c", "test/source-changed");
  await dialog.getByRole("button", { name: "Check again", exact: true }).click();
  await dialog.getByText("The source branch changed. Reopen the merge dialog.", { exact: true }).waitFor();
  assert(await dialog.locator("#dialog-footer .primary").isDisabled());
  await dialog.locator("#dialog-footer button").first().click();
  gitAt(source.directory, "switch", sourceBranch);
  const privateMerges = path.join(profile, "merge-workspaces");
  assert.equal(fs.readdirSync(privateMerges).length, 0);
  assert.equal(gitAt(folder, "worktree", "list", "--porcelain").match(/worktree /g).length, 2);
  await stop();
  // Simulate an older nested-project session whose checkout was placed at the repository root.
  const component = path.join(folder, "component");fs.mkdirSync(component, { recursive: true });
  const metadataPath = path.join(profile, "workspace.json");
  const metadata = JSON.parse(fs.readFileSync(metadataPath, "utf8"));
  metadata.projects.find(item => item.id === project.id).directory = component;
  fs.writeFileSync(metadataPath, JSON.stringify(metadata));
  await start("", profile);
  const legacyHead = gitAt(folder, "rev-parse", "HEAD").trim();
  for (const action of ["merge_targets", "merge_target_preview", "merge_target_apply", "merge_target_resolve"])
    await assert.rejects(api("git", action, { ...context, target: "main", stamp: "0".repeat(64) }), /eski oturumun/);
  assert.equal(gitAt(folder, "rev-parse", "HEAD").trim(), legacyHead);
  await stop();
  console.log("PASS target-branch merge from one source session, unchecked branch, dirty/live target guards, in-session conflict resolution and legacy workspace rejection");
}

async function testRepositoryConcurrency() {
  const base = path.join(temporary, "repository-concurrency");
  const main = path.join(base, "main"), peer = path.join(base, "peer"), other = path.join(base, "other");
  const hooks = path.join(base, "hooks"), ready = path.join(base, "ready"), release = path.join(base, "release");
  fs.mkdirSync(hooks, { recursive: true });
  const gitAt = (cwd, ...args) => cp.execFileSync("git", args, { cwd, encoding: "utf8", windowsHide: true });
  const initialize = folder => {
    fs.mkdirSync(folder);
    gitAt(folder, "init", "-b", "main");
    gitAt(folder, "config", "user.name", "Workflow Test");
    gitAt(folder, "config", "user.email", "test@localhost");
    gitAt(folder, "config", "commit.gpgsign", "false");
    gitAt(folder, "config", "core.autocrlf", "false");
    gitAt(folder, "config", "core.hooksPath", hooks);
    fs.writeFileSync(path.join(folder, "file.txt"), "base\n");
    gitAt(folder, "add", "file.txt");gitAt(folder, "commit", "-m", "Synthetic concurrency baseline");
  };
  for (const folder of [main, other]) initialize(folder);
  gitAt(main, "worktree", "add", "-b", "peer", peer);
  await start("", path.join(base, "profile"));
  const a = await api("app", "add_project", { directory: main });
  const aPeer = await api("app", "add_project", { directory: peer });
  const b = await api("app", "add_project", { directory: other });
  assert.notEqual(a.id, aPeer.id);
  const source = await api("app", "create_session", { projectId: a.id, provider: "codex" });
  const peerSession = await api("app", "create_session", { projectId: aPeer.id, provider: "codex", workspaceMode: "direct" });
  const bSession = await api("app", "create_session", { projectId: b.id, provider: "codex", workspaceMode: "direct" });
  fs.writeFileSync(path.join(source.directory, "file.txt"), "merged source\n");
  gitAt(source.directory, "add", "file.txt");gitAt(source.directory, "commit", "-m", "Synthetic concurrent merge");
  const context = { projectId: a.id };
  const peerContext = { projectId: aPeer.id, sessionId: peerSession.id, path: "file.txt" };
  const bContext = { projectId: b.id, sessionId: bSession.id, path: "file.txt" };
  const peerDraft = await api("files", "edit_read", peerContext), bDraft = await api("files", "edit_read", bContext);
  const plan = await api("git", "state", context);
  const sourceBranch = gitAt(source.directory, "branch", "--show-current").trim();
  const sh = text => "'" + text.replace(/'/g, "'\\''") + "'";
  const posix = file => file.replace(/\\/g, "/");
  fs.writeFileSync(path.join(hooks, "post-merge"), [
    "#!/bin/sh",
    `if [ "$(git rev-parse --show-toplevel)" = ${sh(posix(main))} ]; then`,
    `  printf ready > ${sh(posix(ready))}`,
    "  i=0",
    `  while [ ! -f ${sh(posix(release))} ] && [ "$i" -lt 600 ]; do sleep 0.1; i=$((i+1)); done`,
    "fi", "",
  ].join("\n"));
  const requests = [];
  const request = (domain, action, args) => {
    const state = { done: false, value: undefined, error: null };
    state.promise = api(domain, action, args).then(value => { state.value = value; state.done = true; }, error => { state.error = error; state.done = true; });
    requests.push(state);return state;
  };
  const result = async state => { await state.promise;if (state.error) throw state.error;return state.value; };
  try {
    const merge = request("git", "merge", { ...context, target: sourceBranch, stamp: plan.stamp });
    await until(() => fs.existsSync(ready), "real target post-merge hook", 15000);
    assert.equal(merge.done, false);
    // Queue the same-repository operations first: they must not hold the app-wide lock.
    const peerStart = request("terminal", "start", { id: peerSession.id });
    const peerSave = request("files", "edit_save", { ...peerContext, revision: peerDraft.revision, text: "peer saved\n" });
    await sleep(150);
    const bStart = request("terminal", "start", { id: bSession.id });
    const bSave = request("files", "edit_save", { ...bContext, revision: bDraft.revision, text: "other saved\n" });
    await until(() => bStart.done && bSave.done, "unrelated terminal and editor while Git hook waits", 8000);
    assert((await result(bStart)).pid > 0);assert.equal(await result(bSave), true);
    assert.equal(fs.readFileSync(path.join(other, "file.txt"), "utf8"), "other saved\n");
    assert.equal(fs.existsSync(release), false);
    assert.equal(merge.done, false);assert.equal(peerStart.done, false);assert.equal(peerSave.done, false);
    fs.writeFileSync(release, "release\n");
    assert.equal((await result(merge)).state.head, gitAt(source.directory, "rev-parse", "HEAD").trim());
    assert((await result(peerStart)).pid > 0);assert.equal(await result(peerSave), true);
    assert.equal(gitAt(main, "rev-parse", "HEAD").trim(), gitAt(source.directory, "rev-parse", "HEAD").trim());
    assert.equal(fs.readFileSync(path.join(main, "file.txt"), "utf8"), "merged source\n");
    assert.equal(fs.readFileSync(path.join(peer, "file.txt"), "utf8"), "peer saved\n");
    const pendingFolder = path.join(base, "pending");initialize(pendingFolder);
    const pendingProject = await api("app", "add_project", { directory: pendingFolder });
    fs.unlinkSync(ready);fs.unlinkSync(release);
    fs.writeFileSync(path.join(hooks, "post-checkout"), [
      "#!/bin/sh", `printf ready > ${sh(posix(ready))}`, "i=0",
      `while [ ! -f ${sh(posix(release))} ] && [ "$i" -lt 600 ]; do sleep 0.1; i=$((i+1)); done`, "",
    ].join("\n"));
    const creation = request("app", "create_session", { projectId: pendingProject.id, provider: "codex" });
    await until(() => fs.existsSync(ready), "workspace preparation hook", 15000);
    const removed = request("app", "remove_project", { id: pendingProject.id });
    await until(() => removed.done, "metadata while workspace hook waits", 8000);await result(removed);
    assert.equal(creation.done, false);
    fs.writeFileSync(release, "release\n");await creation.promise;
    assert(creation.error);assert.match(String(creation.error), /Projeyi önce|Add the project/);
    assert.equal((await api("app", "state")).sessions.filter(s => s.projectId === pendingProject.id).length, 0);
    assert.equal(gitAt(pendingFolder, "worktree", "list", "--porcelain").match(/worktree /g).length, 1);
    assert.equal(gitAt(pendingFolder, "for-each-ref", "--format=%(refname)", "refs/heads/workflow/"), "");
  } finally {
    fs.writeFileSync(release, "release\n");
    await Promise.all(requests.map(state => state.promise));
    await stop();
  }
  console.log("PASS repository-scoped Git locking: real target hook blocks linked-worktree writes, independent project terminal and save finish before release");
}

async function testRepositoryWithoutGit() {
  const base = path.join(temporary, "repository-without-git"), folder = path.join(base, "project"), emptyPath = path.join(base, "empty-path");
  fs.mkdirSync(folder, { recursive: true });fs.mkdirSync(emptyPath);
  const gitAt = (...args) => cp.execFileSync("git", args, { cwd: folder, encoding: "utf8", windowsHide: true });
  gitAt("init", "-b", "main");
  fs.writeFileSync(path.join(folder, "file.txt"), "source\n");
  gitAt("add", "file.txt");
  gitAt("-c", "core.hooksPath=" + emptyPath, "-c", "commit.gpgsign=false", "-c", "user.name=Fixture", "-c", "user.email=fixture@localhost", "commit", "-m", "Synthetic Git optional baseline");
  await start("", path.join(base, "profile"), { PATH: emptyPath, Path: emptyPath });
  const project = await api("app", "add_project", { directory: folder });
  const session = await api("app", "create_session", { projectId: project.id, provider: "codex", tools: false });
  assert.equal(session.workspaceKind, "copy");
  const context = { projectId: project.id, sessionId: session.id, path: "file.txt" };
  const draft = await api("files", "edit_read", context);
  assert.equal(await api("files", "edit_save", { ...context, revision: draft.revision, text: "saved without Git\n" }), true);
  assert((await api("terminal", "start", { id: session.id })).pid > 0);
  assert.equal(fs.readFileSync(path.join(session.directory, "file.txt"), "utf8"), "saved without Git\n");
  assert.equal(fs.readFileSync(path.join(folder, "file.txt"), "utf8"), "source\n");
  assert.equal((await api("git", "state", context)).available, false);
  assert.equal((await api("git", "current_branch", context)).branch, null, "missing Git falls back without claiming a branch");
  await stop();
  console.log("PASS Git optional: missing executable preserves copy workspaces, terminal startup and editor saves in an existing repository");
}

async function testBaselineSnapshots() {
  const profile = path.join(temporary, "baseline-snapshot-profile");
  const original = Buffer.from("First line\r\nOriginal line\r\n", "utf8");
  const image = Buffer.from([0, 255, 42, 13, 10, 128]);
  const digest = bytes => createHash("sha256").update(bytes).digest("hex");
  const git = (folder, ...args) => cp.execFileSync("git", args, {
    cwd: folder, encoding: "utf8", windowsHide: true,
  }).trim();
  await start("", profile);
  const skip = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
  if (await skip.isVisible()) await skip.click();
  for (const repository of [false, true]) {
    const folder = path.join(temporary, repository ? "baseline-repository" : "baseline-plain");
    fs.mkdirSync(path.join(folder, "nested"), { recursive: true });
    fs.mkdirSync(path.join(folder, "generated"));
    fs.writeFileSync(path.join(folder, ".workflowignore"), "generated/\n");
    fs.writeFileSync(path.join(folder, "generated/omit.txt"), "generated data\n");
    fs.writeFileSync(path.join(folder, "sample.txt"), original);
    fs.writeFileSync(path.join(folder, "nested/data.bin"), image);
    fs.writeFileSync(path.join(folder, "empty.txt"), "");
    for (let index = 0; index < 96; index++) {
      fs.writeFileSync(path.join(folder, "nested", `source-${index}.txt`), `Synthetic source ${index}\n`);
    }
    if (repository) {
      git(folder, "init", "-b", "snapshot-main");
      git(folder, "add", ".");
      git(folder, "-c", "user.name=Workflow Test", "-c", "user.email=test@localhost", "commit", "-m", "Synthetic snapshot baseline");
    }
    const project = await api("app", "add_project", { directory: folder });
    for (const provider of ["codex", "claude", "copilot"]) {
      for (const workspaceMode of ["direct", "isolated"]) {
        const s = await api("app", "create_session", { projectId: project.id, provider, workspaceMode, tools: false });
        assert.equal(s.workspaceKind, workspaceMode === "direct" ? "direct" : repository ? "worktree" : "copy");
        const file = path.join(s.directory, "sample.txt");
        const baselinePath = path.join(profile, "workspaces", s.id, "baseline.json");
        const baselineBytes = fs.readFileSync(baselinePath);
        const baseline = JSON.parse(baselineBytes);
        assert.equal(baseline["sample.txt"], digest(original));
        assert.equal(baseline["nested/data.bin"], digest(image));
        assert.equal(baseline["empty.txt"], digest(Buffer.alloc(0)));
        assert.equal(Object.keys(baseline).length, 100);
        assert(!Object.keys(baseline).some(name => name.startsWith("generated/")));
        for (const [name, hash] of Object.entries(baseline)) {
          const stored = fs.readFileSync(path.join(profile, "objects", hash));
          assert.equal(digest(stored), hash);
          assert.deepEqual(fs.readFileSync(path.join(s.directory, name)), stored);
        }
        const changed = Buffer.from(`Changed by ${provider} in ${workspaceMode}\n`);
        fs.writeFileSync(file, changed);
        const diff = await api("files", "diff", { sessionId: s.id, path: "sample.txt" });
        const restore = { sessionId: s.id, path: "sample.txt", stamp: diff.stamp };
        if (!repository && provider === "codex" && workspaceMode === "direct") {
          const journal = path.join(profile, "undo");
          fs.writeFileSync(journal, "block journal creation");
          await assert.rejects(api("files", "restore", restore));
          assert.deepEqual(fs.readFileSync(file), changed, "failed undo journal preserves the current file");
          fs.unlinkSync(journal);
          const object = path.join(profile, "objects", baseline["sample.txt"]);
          fs.writeFileSync(object, "damaged synthetic backup");
          try {
            await assert.rejects(api("files", "restore", restore), /bütünlüğü/);
            assert.deepEqual(fs.readFileSync(file), changed, "corrupt backup cannot overwrite the current file");
          } finally { fs.writeFileSync(object, original); }
        }
        const undo = await api("files", "restore", restore);
        assert.deepEqual(fs.readFileSync(file), original);
        await api("files", "undo_restore", { sessionId: s.id, token: undo.id });
        assert.deepEqual(fs.readFileSync(file), changed);
        fs.writeFileSync(file, original);
        const binary = path.join(s.directory, "nested/data.bin");
        fs.writeFileSync(binary, Buffer.from([1, 2, 3]));
        const binaryDiff = await api("files", "diff", { sessionId: s.id, path: "nested/data.bin" });
        await api("files", "restore", { sessionId: s.id, path: "nested/data.bin", stamp: binaryDiff.stamp });
        assert.deepEqual(fs.readFileSync(binary), image);
        for (let attempt = 0; attempt < 2; attempt++) {
          const run = await api("terminal", "start", { id: s.id });
          await until(async () => Buffer.from((await api("terminal", "attach", { id: s.id })).data, "base64").toString().includes(`Workflow terminal test · ${provider}`), provider + " startup with captured baseline");
          await api("terminal", "stop", { id: s.id, runId: run.runId });
          await until(async () => !(await api("app", "state")).active.some(row => row.id === s.id), provider + " stopped");
        }
        assert.deepEqual(fs.readFileSync(baselinePath), baselineBytes, "starting and resuming never replaces the original baseline");
        assert.deepEqual(fs.readFileSync(path.join(folder, "sample.txt")), original);
      }
    }
  }
  await stop();
  const failedProfile = path.join(temporary, "baseline-failure-profile");
  const failedSource = path.join(temporary, "baseline-failure-source");
  fs.mkdirSync(failedSource);
  fs.writeFileSync(path.join(failedSource, "sample.txt"), "synthetic source\n");
  await start("", failedProfile);
  const project = await api("app", "add_project", { directory: failedSource });
  fs.writeFileSync(path.join(failedProfile, "objects"), "block backup storage");
  for (const provider of ["codex", "claude", "copilot"]) {
    await assert.rejects(api("app", "create_session", { projectId: project.id, provider, workspaceMode: "direct", tools: false }));
  }
  assert.equal((await api("app", "state")).sessions.length, 0, "failed capture never publishes a session");
  assert.deepEqual(fs.readFileSync(path.join(failedSource, "sample.txt")), Buffer.from("synthetic source\n"));
  await stop();
  console.log("PASS single-read snapshots for all providers across direct, copy and Git worktree modes; exact backups, text/binary restore, undo, restart and fail-closed persistence");
}

async function testTerminalLocations() {
  const profile = path.join(temporary, "terminal-location-profile");
  const base = path.join(temporary, "WorkspaceProjects");
  const repository = path.join(base, "Sample App");
  const plain = path.join(base, "Plain Project");
  fs.mkdirSync(repository, { recursive: true });
  fs.mkdirSync(plain);
  fs.writeFileSync(path.join(repository, "sample.txt"), "Synthetic workspace\n");
  const git = (directory, ...args) => cp.execFileSync("git", args, {
    cwd: directory, encoding: "utf8", windowsHide: true,
  }).trim();
  git(repository, "init", "-b", "feature/footer");
  git(repository, "add", "sample.txt");
  git(repository, "-c", "user.name=Workflow Test", "-c", "user.email=test@localhost", "commit", "-m", "Synthetic workspace");
  await start("", profile);
  const skip = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
  if (await skip.isVisible()) await skip.click();
  const p = await api("app", "add_project", { directory: repository });
  const rows = [];
  for (const provider of ["codex", "claude", "copilot"]) {
    rows.push(await api("app", "create_session", { projectId: p.id, provider, workspaceMode: "direct", tools: false }));
  }
  const refresh = async () => page.evaluate(async () => {
    await window.workflow.refresh();
    await refreshTerminalLocations(true);
  });
  const footer = id => page.evaluate(id => {
    const v = window.workflow.views.get(id);
    return { text: v?.pathValue.textContent, title: v?.path.title, kind: v?.path.dataset.kind, modeHidden: v?.pathMode.hidden };
  }, id);
  await refresh();
  for (const row of rows) {
    assert.deepEqual(await footer(row.id), { text: "feature/footer", title: row.directory, kind: "branch", modeHidden: false });
  }
  git(repository, "switch", "-c", "feature/new-footer");
  await until(async () => (await footer(rows[0].id)).text === "feature/new-footer", "footer follows external branch changes", 13000);
  for (const row of rows) assert.equal((await footer(row.id)).text, "feature/new-footer");
  const isolated = await api("app", "create_session", { projectId: p.id, provider: "claude", tools: false });
  await refresh();
  assert.equal((await footer(isolated.id)).text, git(isolated.directory, "symbolic-ref", "--short", "HEAD"), "isolated session shows its own worktree branch");
  git(repository, "checkout", "--detach");
  await refresh();
  for (const row of rows) {
    assert.deepEqual(await footer(row.id), { text: path.join("WorkspaceProjects", "Sample App"), title: row.directory, kind: "folder", modeHidden: true });
  }
  const plainProject = await api("app", "add_project", { directory: plain });
  for (const provider of ["codex", "claude", "copilot"]) {
    const row = await api("app", "create_session", { projectId: plainProject.id, provider, workspaceMode: "direct", tools: false });
    await page.evaluate(async id => { await window.workflow.refresh(); window.workflow.selectSession(id, true); await refreshTerminalLocations(true); }, row.id);
    assert.deepEqual(await footer(row.id), { text: path.join("WorkspaceProjects", "Plain Project"), title: row.directory, kind: "folder", modeHidden: true });
  }
  assert(!fs.existsSync(path.join(plain, ".git")), "footer lookup must never initialize Git");
  const copy = await api("app", "create_session", { projectId: plainProject.id, provider: "copilot", tools: false });
  await refresh();
  assert.deepEqual(await footer(copy.id), { text: path.join("WorkspaceProjects", "Plain Project"), title: copy.directory, kind: "folder", modeHidden: true }, "isolated copies retain project context while hovering shows the actual working folder");
  assert(await page.evaluate(() => {
    return shortTerminalPath("C:\\long\\Projects\\App\\src", "C:\\long\\Projects\\App") === "Projects\\App\\src"
      && shortTerminalPath("C:\\isolated\\copy", "C:\\long\\Projects\\App") === "Projects\\App"
      && shortTerminalPath("C:\\", "C:\\") === "C:\\"
      && shortTerminalPath("\\\\server\\share\\App", "\\\\server\\share\\App") === "share\\App";
  }), "short paths preserve useful parent context, nested folders, drive roots and network shares");
  await stop();
  console.log("PASS compact terminal footers for all providers, live branch changes, worktrees, detached HEAD, no Git and full-path hover");
}

async function testModelSettings() {
  const profile = path.join(temporary, "model-settings-profile");
  const folder = path.join(temporary, "model-settings-project");
  fs.mkdirSync(folder);
  fs.writeFileSync(path.join(folder, "model-dialog-fixture"), "");
  await start("", profile);
  const setup = page.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
  if (await setup.isVisible()) await setup.click();
  const p = await api("app", "add_project", { directory: folder });
  const dialog = page.locator("#dialog");
  const open = async id => {
    await page.evaluate(async id => {
      await window.workflow.refresh();
      window.workflow.selectSession(id, true);
      window.workflow.views.get(id).model.click();
    }, id);
    await dialog.getByRole("heading", { name: "Select model", exact: true }).waitFor();
  };
  const requests = provider => fs.readFileSync(path.join(folder, `model-requests-${provider}.txt`), "utf8").trim().split("\n").length;
  let claude, copilot;
  for (const provider of ["codex", "claude", "copilot"]) {
    const s = await api("app", "create_session", { projectId: p.id, provider, workspaceMode: "direct", tools: false });
    if (provider === "claude") claude = s;
    if (provider === "copilot") copilot = s;
    await open(s.id);
    assert(await dialog.getByText("Loading model list…", { exact: true }).isVisible(), "dialog opens before the slow provider response");
    assert(await dialog.getByLabel("Model", { exact: true }).isDisabled());
    const efforts = dialog.getByLabel("Reasoning effort", { exact: true });
    assert.equal(await efforts.isVisible(), true);
    await page.locator("#dialog-close").click();
    await page.evaluate(() => modal("Replacement dialog", el("p", "", "Replacement content")));
    await api("models", "list", { id: s.id });
    await dialog.getByRole("heading", { name: "Replacement dialog" }).waitFor();
    assert.equal(requests(provider), 1, "concurrent requests share the same CLI lookup");
    await page.locator("#dialog-close").click();
    const warm = Date.now();
    await open(s.id);
    await until(async () => !await dialog.getByLabel("Model", { exact: true }).isDisabled(), "cached catalog visible");
    assert(Date.now() - warm < 1600, "cached dialog must not wait for the two-second provider lookup");
    assert.equal(requests(provider), 1);
    const approvals = dialog.getByLabel("Approval level", { exact: true });
    if (provider === "claude") {
      assert.equal(await efforts.inputValue(), "", "missing saved effort must preserve the provider default");
      await efforts.selectOption("high");
      await approvals.selectOption("plan");
    } else if (provider === "codex") {
      await approvals.selectOption("manual");
    } else {
      assert(await approvals.isDisabled(), "Copilot live permissions require a running ready terminal");
      assert.equal(await efforts.inputValue(), "medium");
      await dialog.getByLabel("Model", { exact: true }).selectOption("fixture-copilot-basic");
      assert(await efforts.isVisible());
      assert(await efforts.isDisabled(), "unsupported models must not invent effort options");
      await dialog.getByLabel("Model", { exact: true }).selectOption("fixture-copilot");
      await efforts.selectOption("high");
    }
    await dialog.getByRole("button", { name: "Apply", exact: true }).click();
    await dialog.waitFor({ state: "hidden" });
    const saved = (await api("app", "state")).sessions.find(row => row.id === s.id);
    if (provider !== "codex") assert.equal(saved.effort, "high");
    if (provider === "copilot") await assert.rejects(api("models", "apply", { id: s.id, model: "fixture-copilot", effort: "ultra" }), /Düşünme seviyesi geçersiz/);
    if (provider !== "copilot") assert.equal(saved.approvalMode, provider === "claude" ? "plan" : "manual");
    await open(s.id);
    await until(async () => !await dialog.getByLabel("Model", { exact: true }).isDisabled(), "saved model catalog");
    if (provider !== "codex") assert.equal(await efforts.inputValue(), "high");
    await page.locator("#dialog-close").click();
  }
  await page.evaluate(id => window.workflow.selectSession(id, true), claude.id);
  await page.locator("#session-chat-view").click();
  assert(await page.locator("#chat-effort").isVisible(), "Claude reasoning control is available in Chat too");
  await page.locator("#session-terminal-view").click();
  fs.writeFileSync(path.join(folder, "missing-effort-fixture"), "");
  await open(claude.id);
  await until(async () => !await dialog.getByLabel("Model", { exact: true }).isDisabled(), "cached Claude models");
  await dialog.getByRole("button", { name: "Refresh model list", exact: true }).click();
  await dialog.getByText(/The provider did not report reasoning efforts/).waitFor();
  assert(await dialog.getByLabel("Reasoning effort", { exact: true }).isVisible());
  assert(await dialog.getByLabel("Reasoning effort", { exact: true }).isDisabled());
  await assert.rejects(api("models", "apply", { id: claude.id, model: "fixture-claude", effort: "high" }), /Düşünme seviyesi geçersiz/);
  fs.unlinkSync(path.join(folder, "missing-effort-fixture"));
  await dialog.getByRole("button", { name: "Refresh model list", exact: true }).click();
  await until(async () => !await dialog.getByLabel("Reasoning effort", { exact: true }).isDisabled(), "explicit refresh restores reported efforts");
  await page.locator("#dialog-close").click();
  const config = path.join(profile, "test-claude-home", "settings.json");
  fs.mkdirSync(path.dirname(config), { recursive: true });
  fs.writeFileSync(config, JSON.stringify({ model: "fixture-claude" }));
  await assert.rejects(api("models", "apply", { id: claude.id, model: "fixture-claude", effort: "high" }), /Model listesi değişti/);
  await api("models", "list", { id: claude.id });
  fs.writeFileSync(path.join(folder, "model-error-fixture"), "");
  await assert.rejects(api("models", "list", { id: claude.id, refresh: true }), /seçilebilir model bulunamadı/);
  await assert.rejects(api("models", "apply", { id: claude.id, model: "fixture-claude", effort: "high" }), /Önce model listesini aç/);
  fs.unlinkSync(path.join(folder, "model-error-fixture"));
  await api("models", "list", { id: claude.id });
  await stop();
  await start("", profile);
  const restored = (await api("app", "state")).sessions.find(row => row.id === claude.id);
  assert.equal(restored.effort, "high");
  assert.equal(restored.approvalMode, "plan");
  assert.equal((await api("app", "state")).sessions.find(row => row.id === copilot.id).effort, "high");
  await stop();
  console.log("PASS immediate model dialog, scoped/coalesced cache, forced refresh, stale context rejection, terminal approval controls, Claude effort metadata and persistence");
}

async function test() {
  const git = (args) =>
    cp.execFileSync("git", args, {
      cwd: gitProject,
      encoding: "utf8",
      windowsHide: true,
    });
  git(["init"]);
  git(["config", "user.name", "Workflow Test"]);
  git(["config", "user.email", "test@localhost"]);
  fs.writeFileSync(path.join(gitProject, "a.txt"), "initial\n");
  git(["add", "--", "a.txt"]);
  git(["-c", "core.hooksPath=", "commit", "-m", "Initial test"]);
  const remote = path.join(temporary, "remote.git");
  git(["clone", "--bare", gitProject, remote]);
  git(["--git-dir=" + remote, "update-server-info"]);
  server = http.createServer((req, res) => {
    const pathname = new URL(req.url, "http://localhost").pathname;
    if (pathname.startsWith("/repo.git/")) {
      const file = path.resolve(
        remote,
        decodeURIComponent(pathname.slice("/repo.git/".length)),
      );
      if (
        !file.startsWith(remote + path.sep) ||
        !fs.existsSync(file) ||
        !fs.statSync(file).isFile()
      ) {
        res.statusCode = 404;
        res.end();
        return;
      }
      res.setHeader("content-type", "application/octet-stream");
      res.end(fs.readFileSync(file));
      return;
    }
    if (req.url === "/usage") {
      usageRequests++;
      const managed = req.headers.authorization === "Bearer " + managedFixtureCredential;
      if (!managed)
        assert.equal(req.headers.authorization, "Bearer " + fixtureCredential);
      if (usageSwitch)
        fs.writeFileSync(
          path.join(usageRoot, "auth-status.json"),
          JSON.stringify({ ...usageAuth, orgId: "changed" }),
        );
      res.setHeader("content-type", "application/json");
      res.statusCode = usageStatus;
      res.end(
        JSON.stringify(
          usageStatus === 200
            ? {
                five_hour: {
                  utilization: managed ? 30 : 15,
                  resets_at: "2033-05-18T03:33:20Z",
                },
                seven_day: {
                  utilization: 45,
                  resets_at: "2033-05-18T03:33:20Z",
                },
              }
            : { error: "private-service-detail" },
        ),
      );
      return;
    }
    if (req.url === "/api") {
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify({ items: [{ id: 1, name: "Ada" }], ready: true }));
      return;
    }
    res.setHeader("content-type", "text/html");
    res.end(
      '<!doctype html><html><head><title>Workflow Browser Test</title></head><body style="font:18px sans-serif;padding:25px"><h1>Browser test</h1><label>Name <input id="name"></label><button id="save" onclick="document.querySelector(\'#result\').textContent=document.querySelector(\'#name\').value">Save</button><p id="result"></p><select id="role"><option>User</option><option>Admin</option></select><input id="check" type="checkbox"><input id="note" aria-label="Note"></body></html>',
    );
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  if (process.env.WORKFLOW_TEST_MODELS_ONLY === "1") {
    await testModelSettings();
    await testCodexChat();
    await testAdditionalProviderChats();
    return;
  }
  if (process.env.WORKFLOW_TEST_BASELINE_ONLY === "1") {
    await testBaselineSnapshots();
    return;
  }
  if (process.env.WORKFLOW_TEST_INTERACTIONS_ONLY === "1") {
    await testPendingChatInteraction();
    return;
  }
  if (process.env.WORKFLOW_TEST_LOCATIONS_ONLY === "1") {
    await testTerminalLocations();
    await testRepositoryWithoutGit();
    return;
  }
  if (process.env.WORKFLOW_TEST_NOTIFICATIONS_ONLY === "minimized") {
    await testMinimizedSelectedNotifications();
    return;
  }
  if (process.env.WORKFLOW_TEST_NOTIFICATIONS_ONLY === "existing") {
    await testSessionNotifications();
    await testPendingChatInteraction();
    return;
  }
  if (process.env.WORKFLOW_TEST_NOTIFICATIONS_ONLY === "1") {
    await testMinimizedSelectedNotifications();
    await testSessionNotifications();
    await testPendingChatInteraction();
    return;
  }
  if (process.platform === "win32") {
    await testBaselineSnapshots();
    await testTerminalLocations();
    await testModelSettings();
    await testMinimizedSelectedNotifications();
    await testSessionNotifications();
    await testRepositoryWithoutGit();
    await testRepositoryConcurrency();
    await testCodexChat();
    await testAdditionalProviderChats();
    await testPendingChatInteraction();
    await testArchiveStorage();
    await testTargetBranchMerge();
  }
  if (process.platform === "win32") await testCopilotNpm();
  const address = "http://127.0.0.1:" + server.address().port;
  const started = Date.now();
  await start();
  console.log("PASS native UI ready", Date.now() - started, "ms");
  const language = page.locator("#language-select");
  const settings = page.locator("#app-settings");
  const changeLanguage = async (value) => {
    await settings.click();
    await language.selectOption(value);
    await page.waitForFunction(
      (expected) => document.documentElement.lang === expected && !!window.workflow,
      value,
    );
    assert.equal(await language.count(), 0);
  };
  assert.equal(await page.evaluate(() => localStorage.getItem("workflow-language")), null);
  await page.getByRole("button", { name: "+ New session", exact: true }).waitFor();
  await page.locator(".brand strong").click();
  assert(await page.locator("#dialog").isHidden());
  assert.equal(await settings.getAttribute("aria-label"), "Settings");
  await settings.click();
  await page.getByRole("heading", { name: "Settings", exact: true }).waitFor();
  await page.getByRole("button", { name: "About and updates", exact: true }).click();
  const about = page.locator("#about-updates");
  const updateState = await api("updates", "status");
  await about.getByText("Installed version " + updateState.version, { exact: true }).waitFor();
  assert.equal(await about.getByRole("checkbox", { name: "Update automatically" }).isChecked(), updateState.supported);
  assert(await about.getByRole("button", { name: "Check for updates" }).isVisible());
  await page.evaluate(state => window.paintUpdate(state), { ...updateState, checkedAt: Math.floor(Date.now() / 1000), error: null });
  await about.getByText("No newer version found.", { exact: true }).waitFor();
  const limitedUpdate = { ...updateState, checkedAt: Math.floor(Date.now() / 1000),
    error: "GitHub istek sınırına ulaşıldı. Belirtilen zamandan sonra tekrar dene.",
    errorKind: "rate_limited", retryAt: Math.floor(Date.now() / 1000) + 3600, httpStatus: 429 };
  await page.evaluate(state => window.paintUpdate(state), limitedUpdate);
  await about.getByText("GitHub has limited requests. Try again after the time shown.", { exact: true }).waitFor();
  await about.getByText("Server response: HTTP 429", { exact: true }).waitFor();
  assert.match(await about.innerText(), /Retry after: /);
  assert(!await about.getByText("No newer version found.", { exact: true }).count());
  await page.evaluate(state => window.paintUpdate(state), { ...limitedUpdate,
    error: "İmzalı güncelleme bilgisi henüz yayımlanmamış. GitHub sürümlerini kontrol et.",
    errorKind: "not_published", retryAt: null, httpStatus: null });
  await about.getByText("Signed update information has not been published yet. Check GitHub releases.", { exact: true }).waitFor();
  assert(!await about.getByText("Server response: HTTP 429", { exact: true }).count());
  await page.evaluate(state => window.paintUpdate(state), updateState);
  console.log("PASS update errors show cause and retry time without a false up-to-date result");
  const pendingUpdate = { ...updateState, pendingVersion: "99.0.0" };
  await page.evaluate(state => window.paintUpdate(state), pendingUpdate);
  assert(await about.getByRole("button", { name: "Update and restart" }).isVisible());
  await page.locator("#dialog-footer").getByRole("button", { name: "Close", exact: true }).click();
  const updateNotice = page.locator("#update-notice");
  await updateNotice.getByText("Update 99.0.0 is ready.", { exact: true }).waitFor();
  await updateNotice.getByRole("button", { name: "Update and restart" }).click();
  await page.locator("#toast").getByText("No update has been downloaded yet.", { exact: true }).waitFor();
  await page.locator("#toast button").click();
  await updateNotice.getByRole("button", { name: "Dismiss notification" }).click();
  await page.evaluate(state => window.paintUpdate(state), pendingUpdate);
  assert(await updateNotice.isHidden());
  await page.evaluate(state => window.paintUpdate(state), updateState);
  console.log("PASS update preference, ready notice, and immediate installation action");
  await page.locator(".sidebar-actions").getByRole("button", { name: "Start from template" }).waitFor();
  await page.locator("#show-providers").click();
  await page.locator("#providers-section").getByRole("button", { name: "Refresh workspace" }).waitFor();
  await page.locator("#show-providers").click();
  await changeLanguage("tr");
  await page.getByRole("button", { name: "+ Yeni oturum", exact: true }).waitFor();
  const openTemplateMenu = async () => {
    await page.locator("#session-templates").click();
    await page.locator("#session-template-menu").waitFor({ state: "visible" });
  };
  const openTemplateManager = async () => {
    await openTemplateMenu();
    await page.locator("#manage-session-templates").click();
  };
  console.log("PASS English on first launch, Turkish available in Settings");
  assert.equal(
    await page
      .locator(".sidebar")
      .getByRole("button", { name: "Bağlantılar", exact: true })
      .count(),
    0,
  );
  const widths = () =>
    page.evaluate(() => {
      const rect = (selector) =>
        document.querySelector(selector).getBoundingClientRect();
      return {
        width: rect("#layout").width,
        left: rect(".sidebar").width,
        center: rect(".main").width,
        right: rect(".inspector").width,
        rightEdge: rect(".inspector").right,
        controlsEdge: rect(".panel-toggles").right,
        headerEdge: rect(".main-header").right,
        tabsFit:
          document.querySelector("#panel-tabs").scrollWidth <=
          document.querySelector("#panel-tabs").clientWidth + 1,
      };
    });
  const dragPanel = async (side, x) => {
    const handle = await page.locator("#" + side + "-resizer").boundingBox();
    await page.mouse.move(handle.x + handle.width / 2, handle.y + 120);
    await page.mouse.down();
    await page.mouse.move(x, handle.y + 120, { steps: 4 });
    await page.mouse.up();
  };
  const assertPanelsFit = ({
    width,
    center,
    rightEdge,
    controlsEdge,
    headerEdge,
    tabsFit,
  }) => {
    assert(center >= 399, "center panel minimum");
    assert(rightEdge <= width + 1, "inspector stays inside window");
    assert(
      controlsEdge <= headerEdge + 1,
      "workspace controls stay inside header",
    );
    assert(tabsFit, "inspector tabs fit without horizontal overflow");
  };
  const wide = await widths();
  await dragPanel("right", 4);
  assertPanelsFit(await widths());
  assert.equal((await widths()).left, wide.left);
  await dragPanel("right", wide.width - 3);
  assert.equal((await widths()).right, 310);
  await dragPanel("left", wide.width - 3);
  assertPanelsFit(await widths());
  await dragPanel("left", 3);
  assert.equal((await widths()).left, 180);
  await page.locator("#panel-expand").click();
  assertPanelsFit(await widths());
  await page.locator("#toggle-left").click();
  assert.equal((await widths()).left, 0);
  await page.locator(".rail-tools #app-settings").waitFor();
  await page.locator(".rail-tools #addons").click();
  await page.getByRole("heading", { name: "Eklenti kütüphanesi", exact: true }).waitFor();
  await page.locator("#dialog-close").click();
  await page.locator(".rail-tools #archive").click();
  await page.getByRole("heading", { name: "Oturum geçmişi", exact: true }).waitFor();
  await page.locator("#dialog-close").click();
  assertPanelsFit(await widths());
  await page.locator("#toggle-left").click();
  await page.locator("#toggle-right").click();
  assert.equal((await widths()).right, 0);
  await page.locator("#toggle-right").click();
  assertPanelsFit(await widths());
  await page.evaluate(() =>
    localStorage.setItem(
      "agentloom-widths",
      JSON.stringify({ left: "3000px", right: "3000px" }),
    ),
  );
  await page.reload();
  await until(
    () => page.evaluate(() => !!window.workflow),
    "restored panel widths",
  );
  assertPanelsFit(await widths());
  await page.setViewportSize({ width: 900, height: 740 });
  assertPanelsFit(await widths());
  assert.equal((await widths()).left, 132);
  assert.equal((await widths()).right, 310);
  await openTemplateMenu();
  await page.locator("#session-template-items [data-template-id='builtin:1']").waitFor();
  assert(await page.evaluate(() => {
    const primary = document.querySelector("#new-session").getBoundingClientRect();
    const arrow = document.querySelector("#session-templates").getBoundingClientRect();
    const menu = document.querySelector("#session-template-menu").getBoundingClientRect();
    return Math.abs(primary.top - arrow.top) < 1 && Math.abs(primary.height - arrow.height) < 1 &&
      Math.abs(primary.right - arrow.left) < 1 && Math.abs(menu.left - primary.left) < 1 &&
      menu.right <= innerWidth && menu.bottom <= innerHeight;
  }));
  await page.keyboard.press("Escape");
  await page.locator("#session-template-menu").waitFor({ state: "hidden" });
  const workspaceRefresh = page.locator("#providers-section #refresh");
  const providerToggle = page.locator("#show-providers");
  assert.equal(await page.locator(".main-header #refresh").count(), 0);
  await workspaceRefresh.waitFor({ state: "hidden" });
  await providerToggle.click();
  await workspaceRefresh.waitFor({ state: "visible" });
  assert.equal(await workspaceRefresh.innerText(), "Çalışma alanını yenile");
  await workspaceRefresh.click();
  await until(() => workspaceRefresh.isEnabled(), "workspace refresh finished");
  await providerToggle.click();
  await workspaceRefresh.waitFor({ state: "hidden" });
  const viewportSession = await page.context().newCDPSession(page);
  await viewportSession.send("Emulation.clearDeviceMetricsOverride");
  await viewportSession.detach();
  await page.evaluate(() => {
    localStorage.removeItem("agentloom-widths");
    document.documentElement.style.removeProperty("--sidebar");
    document.documentElement.style.removeProperty("--inspector");
  });
  console.log(
    "PASS panel width limits, restore, hidden panels and narrow layout",
  );
  await page.locator('#panel-tabs [data-tab="links"]').click();
  await page.locator(".inspector-header #panel-refresh").click();
  await page.locator("#panel-links").waitFor({ state: "visible" });
  await page.locator('#panel-tabs [data-tab="browser"]').click();
  await page.locator("#show-providers").click();
  const codexQuota = page.locator(
    '[data-provider="codex"] [data-window="codex:primary"] [role="progressbar"]',
  );
  const claudeQuota = page.locator(
    '[data-provider="claude"] [data-window="five_hour"] [role="progressbar"]',
  );
  const copilotQuota = page.locator(
    '[data-provider="copilot"] [data-window="premium_interactions"] [role="progressbar"]',
  );
  await until(
    async () => (await codexQuota.getAttribute("aria-valuenow")) === "75",
    "Codex five-hour remaining quota",
  );
  await until(
    async () => (await claudeQuota.getAttribute("aria-valuenow")) === "85",
    "Claude OAuth quota",
  );
  await until(
    async () => (await copilotQuota.getAttribute("aria-valuenow")) === "40",
    "Copilot remaining quota",
  );
  assert.equal(
    await page
      .locator(
        '[data-provider="codex"] [data-window="codex:secondary"] [role="progressbar"]',
      )
      .getAttribute("aria-valuenow"),
    "20",
  );
  assert(
    (await page.locator('[data-provider="copilot"]').textContent()).includes(
      "Sınırsız",
    ),
  );
  for (const provider of ["codex", "claude", "copilot"]) {
    const card = page.locator('[data-provider="' + provider + '"]');
    await card.getByText("Hesap bağlı").waitFor({ state: "visible" });
    assert.equal(
      await card.getByRole("button", { name: "Hesabı bağla" }).count(),
      0,
    );
  }
  const normalized = await api("app", "provider_usage", { provider: "claude" });
  assert.equal(normalized.state, "ready");
  assert.equal(normalized.connection, "connected");
  assert(!JSON.stringify(normalized).includes(fixtureCredential));
  const beforeRequests = usageRequests;
  fs.writeFileSync(
    path.join(usageRoot, "auth-status.json"),
    JSON.stringify({ ...usageAuth, authMethod: "api_key" }),
  );
  assert.equal(
    (await api("app", "provider_usage", { provider: "claude" })).state,
    "unsupported",
  );
  assert.equal(
    usageRequests,
    beforeRequests,
    "an API-key login must not use stale OAuth credentials",
  );
  fs.writeFileSync(
    path.join(usageRoot, "auth-status.json"),
    JSON.stringify(usageAuth),
  );
  usageSwitch = true;
  const switched = await api("app", "provider_usage", { provider: "claude" });
  assert.equal(switched.state, "unavailable");
  assert.equal(switched.connection, "unknown");
  assert.equal(switched.windows.length, 0);
  usageSwitch = false;
  fs.writeFileSync(
    path.join(usageRoot, "auth-status.json"),
    JSON.stringify(usageAuth),
  );
  usageStatus = 429;
  await page
    .locator('[data-provider="claude"]')
    .getByTitle("Kullanımı yenile", { exact: true })
    .click();
  await until(
    async () =>
      (await page.locator('[data-provider="claude"] .usage-window').count()) ===
        0 &&
      (await page.locator('[data-provider="claude"]').textContent()).includes(
        "Biraz sonra",
      ),
    "rate-limited quota is unavailable rather than exhausted",
  );
  assert(
    !(await page.locator('[data-provider="claude"]').textContent()).includes(
      "private-service-detail",
    ),
  );
  await page
    .locator('[data-provider="claude"]')
    .getByText("Hesap bağlı")
    .waitFor({ state: "visible" });
  usageStatus = 200;
  await page
    .locator('[data-provider="claude"]')
    .getByTitle("Kullanımı yenile", { exact: true })
    .click();
  await until(
    async () => (await claudeQuota.getAttribute("aria-valuenow")) === "85",
    "quota refresh recovery",
  );
  fs.writeFileSync(
    path.join(usageRoot, "auth-status.json"),
    JSON.stringify({ loggedIn: false }),
  );
  const claudeCard = page.locator('[data-provider="claude"]');
  await claudeCard.getByTitle("Kullanımı yenile", { exact: true }).click();
  const connect = claudeCard.getByRole("button", {
    name: "Hesabı bağla",
    exact: true,
  });
  await connect.click();
  const dialog = page.locator("#dialog");
  await dialog.getByText("Sağlayıcının giriş adımlarını takip et.").waitFor();
  await dialog.locator("#dialog-footer button").click();
  await connect.click();
  await dialog.getByText("Sağlayıcının giriş adımlarını takip et.").waitFor();
  await dialog.locator(".xterm-helper-textarea").focus();
  await page.keyboard.type("connect");
  await page.keyboard.press("Enter");
  await dialog.getByText("Hesap bağlı.", { exact: true }).waitFor();
  await dialog.locator("#dialog-footer button").click();
  await claudeCard.getByText("Hesap bağlı").waitFor();
  const afterLogin = await api("app", "state");
  assert.equal(afterLogin.projects.length, 0);
  assert.equal(afterLogin.sessions.length, 0);
  assert.equal(afterLogin.active.length, 0);
  assert(!fs.existsSync(path.join(data, "transcripts")));
  console.log(
    "PASS project-free login, cancel and retry, verified connected state, no session or transcript",
  );
  fs.mkdirSync(path.join(root, "test-results"), { recursive: true });
  await page.screenshot({
    path: path.join(root, "test-results/Provider-quotas.png"),
  });
  await page.locator("#show-providers").click();
  console.log(
    "PASS provider quotas: three transports, five-hour/weekly/remaining/unlimited UI, account switch, API-key isolation, 429 and refresh recovery",
  );
  // Exercise the actual clone dialog and Git HTTP transport, not a mocked command.
  const cloneParent = path.join(temporary, "Clone parent ç");
  fs.mkdirSync(cloneParent);
  await page.locator("#add-project").click();
  const folderChoice = await page
    .getByRole("button", { name: "Klasörden ekle", exact: true })
    .boundingBox();
  const cloneChoice = await page
    .getByRole("button", { name: "Git’ten klonla", exact: true })
    .boundingBox();
  assert(
    cloneChoice.x - folderChoice.x - folderChoice.width >= 7,
    "project choices have a visible gap",
  );
  await page
    .getByRole("button", { name: "Git’ten klonla", exact: true })
    .click();
  const urlField = page.getByLabel("Git depo adresi", { exact: true });
  await urlField.fill(address + "/repo.git");
  await page
    .getByLabel("Kaydedilecek ana klasör", { exact: true })
    .fill(cloneParent);
  await page
    .getByLabel("Yeni klasör adı", { exact: true })
    .fill("kampüs clone");
  const inputWidth = (await urlField.boundingBox()).width;
  const contentWidth = (await page.locator("#dialog-body > div").boundingBox())
    .width;
  assert(
    inputWidth > contentWidth * 0.9,
    "clone address should fill the form width",
  );
  fs.mkdirSync(path.join(root, "test-results"), { recursive: true });
  await page.screenshot({
    path: path.join(root, "test-results/Clone-dialog.png"),
  });
  await page
    .locator("#dialog-footer")
    .getByRole("button", { name: "Ekle", exact: true })
    .click();
  await until(
    async () => !(await page.locator("#dialog").isVisible()),
    "clone dialog success",
  );
  const destination = path.join(cloneParent, "kampüs clone");
  assert.equal(
    fs
      .readFileSync(path.join(destination, "a.txt"), "utf8")
      .replace(/\r\n/g, "\n"),
    "initial\n",
  );
  assert.equal(
    cp
      .execFileSync("git", ["-C", destination, "rev-parse", "HEAD"], {
        encoding: "utf8",
        windowsHide: true,
      })
      .trim(),
    git(["rev-parse", "HEAD"]).trim(),
  );
  const clonedProject = (await api("app", "state")).projects.find(
    (p) => p.directory === destination,
  );
  assert(clonedProject, "clone stores the compatible Windows project path");
  await assert.rejects(
    api("app", "clone", {
      url: address + "/repo.git",
      parent: cloneParent,
      name: "kampüs clone",
    }),
  );
  assert.equal(
    fs
      .readFileSync(path.join(destination, "a.txt"), "utf8")
      .replace(/\r\n/g, "\n"),
    "initial\n",
  );
  const retainedSession = await api("app", "create_session", {
    projectId: clonedProject.id, provider: "codex", name: "Retained session",
  });
  await api("app", "remove_project", { id: clonedProject.id });
  const reopened = await api("app", "add_project", { directory: destination });
  assert.equal(reopened.id, clonedProject.id);
  assert.equal(reopened.removed, false);
  const savedWorkspace = JSON.parse(fs.readFileSync(path.join(data, "workspace.json"), "utf8"));
  assert.equal(savedWorkspace.projects.find((item) => item.id === reopened.id).removed, false);
  assert(savedWorkspace.sessions.some((item) => item.id === retainedSession.id && item.projectId === reopened.id));
  await page.locator(`#project-switcher [data-project-id="${reopened.id}"]`).click();
  await page.locator("#project-sessions").getByText(retainedSession.name, { exact: true }).waitFor();
  await api("app", "remove_project", { id: reopened.id });
  console.log(
    "PASS clone dialog, existing destination protection, and reopening a removed project with its sessions",
  );
  await assert.rejects(api("app", "add_project", { directory: data }), /veri klasörü/);
  const p = await api("app", "add_project", { directory: project }),
    gp = await api("app", "add_project", { directory: gitProject });
  const editName = "editor-check.txt";
  const editPath = path.join(project, editName);
  const sibling = path.join(project, "editor-check.previous");
  fs.writeFileSync(editPath, "\ufeffÖnce\r\nİkinci\r\n");
  fs.writeFileSync(sibling, "keep");
  const saveText = async (target, text) => {
    const { revision } = await api("files", "edit_read", target);
    await api("files", "edit_save", { ...target, revision, text });
  };
  await page.evaluate(async (id) => {
    await window.workflow.refresh();
    window.workflow.selectProject(id);
    await window.workflow.openPanel("files");
  }, p.id);
  const editButton = page.getByRole("button", { name: "Düzenle", exact: true });
  const editor = dialog.getByRole("textbox", { name: "Dosya içeriği" });
  await page.locator("#file-list").getByRole("button", { name: /editor-check\.txt/ }).click();
  await editButton.click();
  await dialog.getByText(/Proje klasörü/).waitFor();
  assert.equal(await editor.inputValue(), "Önce\nİkinci\n");
  await editor.fill("Yeni\nİkinci\n");
  await editor.press("Control+s");
  await dialog.waitFor({ state: "hidden" });
  await page.locator("#file-preview").getByText(/Yeni/).waitFor();
  assert.equal(fs.readFileSync(editPath, "utf8"), "\ufeffYeni\r\nİkinci\r\n");
  assert.equal(fs.readFileSync(sibling, "utf8"), "keep");
  await editButton.click();
  await editor.fill("Kullanıcının taslağı");
  fs.writeFileSync(editPath, "\ufeffAjanın düzenlemesi\r\n");
  await dialog.getByRole("button", { name: "Kaydet", exact: true }).click();
  await dialog.getByText(/Dosya veya seçili Git dalı değişti/).waitFor();
  assert.equal(await editor.inputValue(), "Kullanıcının taslağı");
  assert.equal(fs.readFileSync(editPath, "utf8"), "\ufeffAjanın düzenlemesi\r\n");
  await editor.press("Escape");
  await dialog.getByText("Kaydedilmemiş düzenlemeler var.").waitFor();
  await dialog.getByRole("button", { name: "Düzenlemeye dön" }).click();
  assert.equal(await editor.inputValue(), "Kullanıcının taslağı");
  await dialog.getByRole("button", { name: "Vazgeç" }).click();
  await dialog.getByRole("button", { name: "Taslağı sil" }).click();
  await dialog.waitFor({ state: "hidden" });
  await assert.rejects(api("files", "edit_read", {
    projectId: gp.id, path: ".git/config",
  }), /düzenlenemiyor/);
  console.log("PASS project file editor, Ctrl+S, UTF-8 BOM and CRLF, draft and external edit protection");
  const addonRoot = path.join(data, "addons/ponytail");
  fs.mkdirSync(addonRoot, { recursive: true });
  fs.writeFileSync(
    path.join(addonRoot, "manifest.json"),
    JSON.stringify({ version: "test" }),
  );
  const sessions = [];
  for (const provider of ["codex", "claude", "copilot"]) {
    const s = await api("app", "create_session", {
      projectId: p.id,
      provider,
      name: provider + " test",
      tools: true,
    });
    sessions.push(s);
    assert.equal(s.workspaceKind, "copy");
    if (provider === "codex") {
      const blockedLog = path.join(data, "transcripts", s.id + ".ansi");
      fs.mkdirSync(blockedLog, { recursive: true });
      try {
        await assert.rejects(api("terminal", "start", { id: s.id }));
        assert(!(await api("app", "state")).active.some((run) => run.id === s.id));
      } finally {
        fs.rmdirSync(blockedLog);
      }
    }
    const snap = await api("terminal", "start", { id: s.id });
    pids.push(snap.pid);
    await until(async () => {
      const t = await api("terminal", "attach", { id: s.id });
      return Buffer.from(t.data, "base64").toString().includes("Ready >");
    }, provider + " PTY");
    await api("terminal", "write", {
      id: s.id,
      runId: snap.runId,
      data: "marker-" + provider + "\r",
    });
  }
  const projectButton = (id) => page.locator(`#project-switcher [data-project-id="${id}"]`);
  const sessionList = page.locator("#project-sessions");
  await projectButton(p.id).click();
  const recentSession = sessionList.locator(`[data-session-id="${sessions[1].id}"]`);
  await recentSession.click();
  const activeRunIds = async () => (await api("app", "state")).active.map((run) => run.runId).sort();
  const beforeSwitch = await activeRunIds();
  assert.equal(await projectButton(p.id).getAttribute("data-running"), "true");
  await projectButton(gp.id).focus();
  await page.keyboard.press("Enter");
  assert.equal(await projectButton(gp.id).getAttribute("aria-pressed"), "true");
  assert.equal(await sessionList.locator(".session-entry").count(), 0);
  await projectButton(p.id).click();
  assert.equal(await sessionList.locator(".session-entry").count(), sessions.length);
  assert.equal(await recentSession.getAttribute("aria-pressed"), "true");
  assert.deepEqual(await activeRunIds(), beforeSwitch);
  console.log("PASS project rail switches projects, scopes sessions, and restores focus without restarting terminals");
  await page.locator("#add-session-folder").click();
  await page.locator("#dialog").getByLabel("Klasör adı").fill("API changes");
  await page.locator("#dialog").getByRole("button", { name: "Klasörü oluştur" }).click();
  const folderRow = sessionList.locator(".session-folder-select").filter({ hasText: "API changes" });
  await folderRow.waitFor();
  await until(async () => (await folderRow.getAttribute("aria-expanded")) === "true", "new folder selected");
  const folderId = await folderRow.getAttribute("data-folder-id");
  const savedFolder = (await api("app", "state")).projects.find((entry) => entry.id === p.id)
    .sessionFolders.find((entry) => entry.id === folderId);
  assert.equal(savedFolder.name, "API changes");
  assert.equal(await sessionList.locator(".session-entry").count(), 0);
  assert(await page.locator("#session-tabs").isHidden());
  await assert.rejects(api("app", "folder_create", { projectId: p.id, name: " api CHANGES " }), /zaten var/);
  await assert.rejects(api("app", "folder_move_session", {
    projectId: p.id, id: sessions[0].id, folderId: "missing-folder",
  }), /bulunamadı/);
  await page.locator("#new-session").click();
  assert.equal(await page.locator("#dialog").getByLabel("Oturum klasörü").inputValue(), folderId);
  await page.locator("#dialog-close").click();
  await sessionList.locator('.session-folder-select[data-folder-id=""]').click();
  await sessionList.locator('[data-session-id="' + sessions[0].id + '"]').click();
  await page.locator('[data-id="' + sessions[0].id + '"] .card-header')
    .getByTitle("Oturum seçenekleri").click();
  await page.locator("#workspace-action-menu").getByRole("menuitem", { name: "Klasöre taşı" }).click();
  await page.locator("#dialog").getByLabel("Oturum klasörü").selectOption(folderId);
  await page.locator("#dialog").getByRole("button", { name: "Taşı", exact: true }).click();
  await until(async () => (await folderRow.getAttribute("aria-expanded")) === "true", "moved session opened in its folder");
  await api("app", "folder_move_session", { projectId: p.id, id: sessions[1].id, folderId });
  await page.evaluate(() => window.workflow.refresh());
  const topicTabs = page.locator("#session-tabs").getByRole("tab");
  assert.equal(await topicTabs.count(), 2);
  assert.equal(await page.locator(".session-card:not([hidden])").count(), 2);
  assert.equal(await page.locator("#browser-session option").count(), 2);
  await topicTabs.first().focus();
  await page.keyboard.press("ArrowRight");
  const activeTab = page.locator('#session-tabs [aria-selected="true"]');
  assert.equal(await activeTab.getAttribute("data-session-id"), sessions[1].id);
  await page.locator("#view-grid").click();
  await page.locator("#viewport").evaluate((viewport) => { viewport.style.maxHeight = "400px"; });
  await page.locator("#cards").evaluate((cards) => { cards.style.gridTemplateColumns = "1fr"; });
  await topicTabs.first().click();
  const lastCard = page.locator('[data-id="' + sessions[1].id + '"].session-card');
  const lastCardVisible = () => lastCard.evaluate((card) => {
    const bounds = card.getBoundingClientRect();
    const viewport = document.querySelector("#viewport").getBoundingClientRect();
    return bounds.top < viewport.bottom && bounds.bottom > viewport.top;
  });
  assert(!(await lastCardVisible()), "the second card starts outside the Grid viewport");
  await topicTabs.last().click();
  assert(await lastCardVisible(), "selecting an offscreen tab reveals its card in Grid view");
  await page.locator("#cards").evaluate((cards) => cards.style.removeProperty("grid-template-columns"));
  await page.locator("#viewport").evaluate((viewport) => viewport.style.removeProperty("max-height"));
  await page.locator("#view-map").click();
  await folderRow.click();
  assert.equal(await folderRow.getAttribute("aria-expanded"), "false");
  assert.equal(await folderRow.getAttribute("aria-current"), "true");
  assert.equal(await sessionList.locator(".session-entry").count(), 0);
  assert.equal(await topicTabs.count(), 2);
  assert.equal(await activeTab.getAttribute("data-session-id"), sessions[1].id);
  assert.equal(await page.locator(".session-card:not([hidden])").count(), 2);
  assert.deepEqual(await activeRunIds(), beforeSwitch);
  await page.reload();
  await until(() => page.evaluate(() => !!window.workflow), "topic view reload");
  assert.equal(await folderRow.getAttribute("aria-expanded"), "false");
  assert.equal(await sessionList.locator(".session-entry").count(), 0);
  await folderRow.focus();
  await page.keyboard.press("Enter");
  assert.equal(await folderRow.getAttribute("aria-expanded"), "true");
  assert.equal(await sessionList.locator(".session-entry").count(), 2);
  assert.equal(await activeTab.getAttribute("data-session-id"), sessions[1].id);
  assert.deepEqual(await activeRunIds(), beforeSwitch);
  await sessionList.getByRole("button", { name: "Klasör ayarları", exact: true }).click();
  await page.locator("#workspace-action-menu").getByRole("menuitem", { name: "Klasörü yeniden adlandır" }).click();
  await page.locator("#dialog").getByLabel("Klasör adı").fill("API review");
  await page.locator("#dialog").getByRole("button", { name: "Kaydet" }).click();
  await sessionList.getByText("API review", { exact: true }).waitFor();
  await sessionList.getByRole("button", { name: "Klasör ayarları", exact: true }).click();
  await page.locator("#workspace-action-menu").getByRole("menuitem", { name: "Klasörü kaldır" }).click();
  await page.locator("#dialog").getByRole("button", { name: "Kaldır", exact: true }).click();
  await until(async () => (await topicTabs.count()) === 3, "removed folder restores sessions to General");
  const restored = (await api("app", "state")).sessions.filter((entry) => sessions.some((s) => s.id === entry.id));
  assert(restored.every((entry) => entry.folderId == null));
  const generalFolder = sessionList.locator('.session-folder-select[data-folder-id=""]');
  await generalFolder.click();
  assert.equal(await generalFolder.getAttribute("aria-expanded"), "false");
  assert.equal(await topicTabs.count(), 3);
  await generalFolder.click();
  assert.equal(await sessionList.locator(".session-entry").count(), 3);
  assert.deepEqual(await activeRunIds(), beforeSwitch);
  console.log("PASS session topic folders, native tabs and keyboard navigation, reload, move and removal without terminal restart");
  const isolatedFile = { sessionId: sessions[0].id, path: editName };
  const isolatedOriginal = (await api("files", "edit_read", isolatedFile)).text;
  await saveText(isolatedFile, "Sadece oturum\n");
  assert.equal(fs.readFileSync(path.join(sessions[0].directory, editName), "utf8"), "\ufeffSadece oturum\r\n");
  assert.equal(fs.readFileSync(editPath, "utf8"), "\ufeffAjanın düzenlemesi\r\n");
  await saveText(isolatedFile, isolatedOriginal);
  await page.evaluate(
    (id) => window.workflow.selectSession(id),
    sessions[0].id,
  );
  await until(
    async () => (await page.locator(".session-card:not([hidden])").count()) === 3,
    "cards",
  );
  await until(
    () => fs.existsSync(path.join(data, "test-input/codex.txt")),
    "fixture input",
  );
  const inputBefore = fs.readFileSync(
    path.join(data, "test-input/codex.txt"),
    "utf8",
  );
  const before = await api("terminal", "attach", { id: sessions[0].id });
  const mapLayout = await page.locator("#cards").evaluate(cards =>
    [...cards.children].map(card => [card.dataset.id, card.style.cssText]));
  const fillsWorkspace = async (id) => page.evaluate(id => {
    const viewport = document.querySelector("#viewport");
    const area = viewport.getBoundingClientRect();
    const card = window.workflow.views.get(id).card.getBoundingClientRect();
    return Math.abs(card.top - area.top - viewport.clientTop) < 2
      && Math.abs(card.left - area.left - viewport.clientLeft) < 2
      && Math.abs(card.width - viewport.clientWidth) < 2
      && Math.abs(card.height - viewport.clientHeight) < 2
      && viewport.scrollHeight === viewport.clientHeight;
  }, id);
  await page.locator("#focus-card").click();
  await until(() => fillsWorkspace(sessions[0].id), "Focus fills the workspace from its top edge");
  assert.equal(await page.locator("#cards .session-card:visible").count(), 1);
  for (const control of ["#zoom-out", "#zoom", "#zoom-in", "#fit", "#focus-card"]) {
    assert(await page.locator(control).isVisible(), "focused Terminal keeps the workspace toolbar visible");
  }
  await page.locator("#zoom-out").click();
  assert(await page.locator("#view-map").evaluate(button => button.classList.contains("selected")));
  const zoomedOut = await page.locator("#zoom").textContent();
  await page.locator("#focus-card").click();
  await page.locator("#zoom-in").click();
  assert(await page.locator("#view-map").evaluate(button => button.classList.contains("selected")));
  assert(parseInt(await page.locator("#zoom").textContent(), 10) > parseInt(zoomedOut, 10));
  await page.locator("#focus-card").click();
  await page.locator("#fit").click();
  assert(await page.locator("#view-map").evaluate(button => button.classList.contains("selected")));
  assert.equal(await page.locator("#cards .session-card:visible").count(), 3);
  await page.locator("#focus-card").click();
  assert(await fillsWorkspace(sessions[0].id));

  await page.locator("#session-chat-view").click();
  await page.locator("#chat-input").fill("Keep my focused session draft.");
  await page.locator("#session-terminal-view").click();
  assert(await fillsWorkspace(sessions[0].id));
  await page.locator(`#session-tabs [data-session-id="${sessions[1].id}"]`).click();
  await until(() => fillsWorkspace(sessions[1].id), "next focused session uses the same full area");
  await page.locator("#toggle-right").click();
  await until(() => fillsWorkspace(sessions[1].id), "focused Terminal fills the area after panel resize");
  await page.locator("#toggle-right").click();
  await page.locator(`#session-tabs [data-session-id="${sessions[0].id}"]`).click();
  await page.locator("#session-chat-view").click();
  assert.equal(await page.locator("#chat-input").inputValue(), "Keep my focused session draft.");
  await page.locator("#session-terminal-view").click();
  await page.locator("#view-map").click();
  assert.deepEqual(await page.locator("#cards").evaluate(cards =>
    [...cards.children].map(card => [card.dataset.id, card.style.cssText])), mapLayout);
  assert.deepEqual(await activeRunIds(), beforeSwitch);
  assert.equal(fs.readFileSync(path.join(data, "test-input/codex.txt"), "utf8"), inputBefore);
  console.log("PASS focused Terminal fills the workspace, follows tabs and panel resize, preserves Chat draft and map layout without input");
  await page.reload();
  await until(() => page.evaluate(() => !!window.workflow), "reload");
  const after = await api("terminal", "attach", { id: sessions[0].id });
  assert.equal(after.runId, before.runId);
  assert.equal(after.pid, before.pid);
  assert.equal(
    fs.readFileSync(path.join(data, "test-input/codex.txt"), "utf8"),
    inputBefore,
  );
  console.log("PASS three provider PTYs and renderer reattach without input");

  const clipboardSession = sessions[0].id;
  const clipboardTerminal = page.locator('[data-id="' + clipboardSession + '"] .xterm-helper-textarea');
  const receivedPath = path.join(data, "test-input", "codex.txt");
  const inputLog = () => fs.readFileSync(receivedPath, "utf8");
  const beforePaste = inputLog();
  assert.equal(await page.evaluate(id => window.workflow.views.get(id).term.modes.bracketedPasteMode, clipboardSession), false);
  const pasteInApp = async (text) => {
    await clipboardTerminal.focus();
    await clipboardTerminal.evaluate((target, text) => {
      const clipboardData = new DataTransfer();
      clipboardData.setData("text/plain", text);
      target.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData }));
    }, text);
  };
  const confirmation = page.getByRole("dialog", { name: "Terminale yapıştır" });
  await pasteInApp("");
  await page.locator("#toast").getByText("Windows panosundan yapıştırılabilir metin alınamadı.", { exact: true }).waitFor();
  assert.equal(inputLog(), beforePaste, "an empty paste must leave fixture input unchanged");
  await page.locator("#toast button").click();
  await pasteInApp("synthetic clipboard\r\nsecond line");
  await confirmation.getByRole("button", { name: "Vazgeç", exact: true }).click();
  await confirmation.waitFor({ state: "hidden" });
  assert.equal(inputLog(), beforePaste, "cancelling paste must leave the fixture input unchanged");
  await pasteInApp("synthetic clipboard\r\nsecond line\tend");
  await confirmation.getByRole("button", { name: "Tek satır olarak yapıştır", exact: true }).click();
  await confirmation.waitFor({ state: "hidden" });
  await until(() => page.evaluate(id => window.workflow.views.get(id).inputDraft === "synthetic clipboard second line end", clipboardSession), "single-line paste handed to PTY");
  assert.equal(inputLog(), beforePaste, "paste must not submit the terminal line");
  await clipboardTerminal.focus();
  await page.keyboard.press("Enter");
  await until(() => inputLog().slice(beforePaste.length).replace(/\r\n/g, "\n") === "synthetic clipboard second line end\n", "exactly one submitted line");
  console.log("PASS native Workflow WebView and provider PTY accept unbracketed multiline paste only as a confirmed single-line draft");
  const teamsRow = page.locator(".teams-control");
  const teamsEdit = teamsRow.getByRole("button", { name: "Teams hedefini düzenle", exact: true });
  assert.equal(await teamsRow.locator("#teams-state").textContent(), "Giriş gerekli");
  assert(await teamsRow.getByRole("button", { name: "Teams’e bağlan" }).isVisible());
  assert(await teamsEdit.isHidden());
  await page.evaluate(() => window.paintTeams({ state: "signing-in" }));
  assert.equal(await teamsRow.locator("#teams-state").textContent(), "Giriş bekleniyor");
  assert(await teamsRow.getByRole("button", { name: "Teams girişine devam et" }).isVisible());
  await page.evaluate(() => window.paintTeams({ state: "connected" }));
  assert.equal(await teamsRow.locator("#teams-state").textContent(), "Bağlı");
  const teamsOpen = teamsRow.getByRole("button", { name: "Teams’i aç", exact: true });
  await teamsOpen.click();
  await page.locator("#toast").getByText("Önce Teams hesabına giriş yap.", { exact: true }).waitFor();
  await teamsRow.locator(".teams-info strong").click();
  assert(await page.locator("#dialog").isHidden());
  await page.evaluate(() => window.paintTeams({ state: "disconnected" }));
  await page.evaluate((id) => window.workflow.selectSession(id), sessions[0].id);
  const teamsDialog = page.locator("#dialog");
  const teamsLink = teamsDialog.locator('input[type="url"]');
  const saveTeamsTarget = teamsDialog.getByRole("button", { name: "Kaydet", exact: true });
  const openTeamsTarget = async () => {
    await page.locator('.session-card[data-id="' + sessions[0].id + '"]')
      .getByTitle("Oturum seçenekleri", { exact: true }).click();
    await page.locator("#workspace-action-menu").getByRole("menuitem", { name: "Teams hedefi", exact: true }).click();
    await teamsDialog.getByRole("heading", { name: "Teams hedefi · " + sessions[0].name, exact: true }).waitFor();
  };
  const savedTeamsTarget = async (id) =>
    (await api("app", "state")).sessions.find((s) => s.id === id).teamsChatLink;
  await openTeamsTarget();
  await teamsLink.fill("https://example.invalid/l/chat/48%3Anotes/conversations");
  await saveTeamsTarget.click();
  await teamsDialog.locator(".dialog-error").waitFor();
  assert(!await savedTeamsTarget(sessions[0].id));
  await teamsDialog.getByRole("button", { name: "Kendi sohbetim", exact: true }).click();
  await saveTeamsTarget.click();
  await teamsDialog.waitFor({ state: "hidden" });
  const savedLink = await savedTeamsTarget(sessions[0].id);
  assert.equal(decodeURIComponent(new URL(savedLink).pathname), "/l/chat/48:notes/conversations");
  assert(!await savedTeamsTarget(sessions[1].id));
  const teamsAttempt = { text: "Local Teams test" };
  assert.match((await mcp(sessions[0].id, "teams_send_message", teamsAttempt)).error, /Önce Teams hesabına giriş yap/);
  assert.match((await mcp(sessions[1].id, "teams_send_message", teamsAttempt)).error, /Teams hedefini kaydet/);
  assert.match((await mcp(sessions[0].id, "teams_open")).error, /Önce Teams hesabına giriş yap/);
  await page.evaluate(() => window.paintTeams({ state: "connected" }));
  await teamsEdit.click();
  assert.equal(await teamsDialog.locator("#dialog-title").textContent(), "Teams hedefi · " + sessions[0].name);
  assert.equal(await teamsLink.inputValue(), savedLink);
  assert.equal(await teamsDialog.locator("textarea").count(), 0);
  const newTeamsLink = "https://teams.microsoft.com/l/chat/19:example@thread.v2/conversations";
  await teamsLink.fill(newTeamsLink);
  await saveTeamsTarget.click();
  await teamsDialog.waitFor({ state: "hidden" });
  assert.equal(new URL(await savedTeamsTarget(sessions[0].id)).pathname, new URL(newTeamsLink).pathname);
  await page.evaluate((id) => window.workflow.selectSession(id), sessions[1].id);
  await teamsEdit.click();
  assert.equal(await teamsDialog.locator("#dialog-title").textContent(), "Teams hedefi · " + sessions[1].name);
  assert.equal(await teamsLink.inputValue(), "");
  await teamsDialog.getByRole("button", { name: "Vazgeç", exact: true }).click();
  await page.evaluate((id) => {
    window.paintTeams({ state: "disconnected" });
    window.workflow.selectSession(id);
  }, sessions[0].id);
  await api("teams", "set_target", { sessionId: sessions[0].id, link: "" });
  console.log("PASS Teams connection states, window open action and session-scoped MCP destination");
  for (const s of sessions) {
    const models = await api("models", "list", { id: s.id });
    assert(models.some((m) => m.id === "fixture-" + s.provider));
  }
  console.log("PASS Codex, Claude and Copilot model catalog protocols");
  const modelStartup = await api("app", "create_session", {
    projectId: p.id,
    provider: "codex",
    name: "Codex startup model",
  });
  fs.writeFileSync(
    path.join(modelStartup.directory, "model-startup-fixture"),
    "",
  );
  await page.evaluate(
    (id) => window.workflow.selectSession(id),
    modelStartup.id,
  );
  await page.locator("#view-focus").click();
  await page.locator("#toggle-right").click();
  const modelView = page.locator(
    '.session-card[data-id="' + modelStartup.id + '"]',
  );
  await modelView
    .getByTitle("Aynı konuşmaya devam et", { exact: true })
    .click();
  const modelScreenHas = (message) =>
    page.evaluate(
      ({ id, message }) => {
        const term = window.workflow.views.get(id)?.term;
        const buffer = term?.buffer.active;
        return (
          !!buffer &&
          Array.from({ length: term.rows }, (_, row) =>
            buffer.getLine(buffer.baseY + row)?.translateToString(true),
          ).some((line) => line?.includes(message))
        );
      },
      { id: modelStartup.id, message },
    );
  await until(
    () => modelScreenHas("Skip until next version"),
    "Codex update screen",
  );
  const modelRun = await api("terminal", "attach", { id: modelStartup.id });
  pids.push(modelRun.pid);
  const modelButton = modelView.getByTitle("Model seç", { exact: true });
  await modelButton.click();
  const modelDialog = page.locator("#dialog");
  await modelDialog.getByText(/Codex güncelleme ekranında bekliyor/).waitFor();
  await modelDialog
    .getByRole("button", { name: "Uygula", exact: true })
    .click();
  await modelDialog
    .locator(".dialog-error")
    .filter({ hasText: /Terminalde Esc/ })
    .waitFor();
  const modelInputs = () =>
    fs.readFileSync(path.join(data, "test-input/codex.txt"), "utf8");
  assert(!modelInputs().includes("/model fixture-codex"));
  await page.locator("#dialog-close").click();
  await modelView.locator(".xterm-helper-textarea").focus();
  await page.keyboard.press("Escape");
  assert.equal(
    await page.evaluate(
      (id) => window.workflow.views.get(id).inputDraft,
      modelStartup.id,
    ),
    "",
  );
  await api("terminal", "write", {
    id: modelStartup.id,
    runId: modelRun.runId,
    data: "skip-update\r",
  });
  await until(
    () => modelScreenHas("Ask Codex to do anything"),
    "Codex first prompt",
  );
  await modelButton.click();
  await modelDialog
    .getByRole("button", { name: "Uygula", exact: true })
    .click();
  await until(
    () => modelInputs().includes("/model fixture-codex"),
    "model sent at first empty prompt",
  );
  await modelDialog.waitFor({ state: "hidden" });
  await api("terminal", "stop", { id: modelStartup.id, runId: modelRun.runId });
  await until(
    async () =>
      (await api("terminal", "attach", { id: modelStartup.id })).exited,
    "model fixture stopped",
  );
  await api("app", "session_patch", { id: modelStartup.id, archived: true });
  await page.locator("#toggle-right").click();
  await page.locator("#view-map").click();
  console.log(
    "PASS model menu explains Codex update and accepts the first empty prompt",
  );
  const login = await api("app", "create_session", {
    projectId: p.id,
    provider: "codex",
    name: "Login regression",
    login: true,
    model: "fixture-codex",
  });
  const loginPath = login.directory;
  await page.evaluate((id) => window.workflow.selectSession(id), login.id);
  const loginCard = page.locator('.session-card[data-id="' + login.id + '"]');
  await until(
    () => loginCard.locator(".card-notice").isVisible(),
    "login notice",
  );
  assert(
    await loginCard
      .getByRole("button", { name: "Konuşmaya geç", exact: true })
      .isVisible(),
  );
  const loginRun = await api("terminal", "start", { id: login.id });
  pids.push(loginRun.pid);
  await assert.rejects(api("app", "conversation_from_login", { id: login.id }));
  await api("terminal", "stop", { id: login.id, runId: loginRun.runId });
  await until(
    async () => (await api("terminal", "attach", { id: login.id })).exited,
    "login stopped",
  );
  const archivedName = "UzunOturumAdı".repeat(18);
  await api("app", "session_patch", {
    id: login.id,
    name: archivedName,
  });
  await page.evaluate(() => window.workflow.refresh());
  await page.evaluate(id => window.workflow.selectSession(id), login.id);
  const loginOptions = page.locator('#project-sessions [data-session-options="' + login.id + '"]');
  await loginOptions.click();
  await page.locator("#workspace-action-menu").getByRole("menuitem", { name: "Arşive taşı", exact: true }).click();
  await loginCard.waitFor({ state: "hidden" });
  assert.equal((await api("app", "state")).sessions.find(s => s.id === login.id).archived, true);
  await page.locator("#archive").click();
  const archived = page
    .locator("#dialog .archive-row")
    .filter({ hasText: archivedName });
  const historyButton = archived.getByRole("button", {
    name: "Geçmiş",
    exact: true,
  });
  const restoreButton = archived.getByRole("button", {
    name: "Geri getir",
    exact: true,
  });
  const bounds = await Promise.all(
    [archived.locator("span"), historyButton, restoreButton, archived].map(
      (item) => item.boundingBox(),
    ),
  );
  const [title, historyBounds, restore, row] = bounds;
  assert(title && historyBounds && restore && row);
  assert(title.x + title.width <= historyBounds.x);
  assert(historyBounds.x + historyBounds.width <= restore.x);
  assert(restore.x + restore.width <= row.x + row.width + 1);
  assert(historyBounds.width > 55 && restore.width > 65);
  assert.match(await archived.locator("xpath=ancestor::details[@data-history-group='archive']").locator(":scope > summary").textContent(), /^Arşiv/);
  await historyButton.click();
  await until(
    async () =>
      (await page.locator("#dialog-title").textContent()) ===
      archivedName + " · Geçmiş",
    "archived conversation history",
  );
  await page.locator("#dialog").getByRole("button", { name: "Oturum geçmişine dön", exact: true }).click();
  await restoreButton.click();
  await loginCard.waitFor({ state: "visible" });
  assert.equal(
    (await api("app", "state")).active.some((a) => a.id === login.id),
    false,
  );
  await page.locator("#archive").click();
  const historyRow = page.locator('#session-history .archive-row[data-session-id="' + login.id + '"]');
  assert.match(await historyRow.locator("..").locator("summary").textContent(), /^Kapalı oturumlar/);
  await historyRow.getByRole("button", { name: "Arşive taşı", exact: true }).click();
  await historyRow.getByRole("button", { name: "Geri getir", exact: true }).waitFor();
  assert.match(await historyRow.locator("xpath=ancestor::details[@data-history-group='archive']").locator(":scope > summary").textContent(), /^Arşiv/);
  await historyRow.getByRole("button", { name: "Geri getir", exact: true }).click();
  await page.locator("#dialog").waitFor({ state: "hidden" });
  await loginCard.waitFor({ state: "visible" });
  assert.equal((await api("app", "state")).sessions.find(s => s.id === login.id).archived, false);
  const secondRun = await api("terminal", "start", { id: login.id });
  pids.push(secondRun.pid);
  await loginOptions.click();
  await page.locator("#workspace-action-menu").getByRole("menuitem", { name: "Arşive taşı", exact: true }).click();
  await page.locator("#dialog").getByRole("button", { name: "Durdur ve arşivle", exact: true }).click();
  await page.locator("#dialog").waitFor({ state: "hidden" });
  assert((await api("terminal", "attach", { id: login.id })).exited);
  await page.locator("#archive").click();
  await historyRow.getByRole("button", { name: "Geri getir", exact: true }).click();
  await loginCard.waitFor({ state: "visible" });
  console.log(
    "PASS session history groups closed and archived sessions, archives through both entry points and restores without launching the terminal",
  );

  await api("app", "session_patch", {
    id: login.id,
    name: "Login regression",
    browserAccess: false,
  });
  await page.evaluate(() => window.workflow.refresh());
  await loginCard
    .getByRole("button", { name: "Konuşmaya geç", exact: true })
    .click();
  await until(
    async () =>
      (await api("app", "state")).active.some((a) => a.id === login.id),
    "converted conversation started",
  );
  const converted = (await api("app", "state")).sessions.find(
    (s) => s.id === login.id,
  );
  assert.equal(converted.login, false);
  assert.equal(converted.directory, loginPath);
  assert.equal(converted.browserAccess, false);
  assert.equal(converted.model, "fixture-codex");
  assert.equal(converted.conversationId, undefined);
  assert(await loginCard.locator(".card-notice").isHidden());
  console.log(
    "PASS login card: explicit conversion, active-login guard, same workspace and permissions, normal conversation starts",
  );

  await assert.rejects(
    api("terminal", "write", {
      id: sessions[0].id,
      runId: "stale",
      data: "bad",
    }),
  );
  const addonSession = await api("app", "create_session", {
    projectId: p.id,
    provider: "codex",
    name: "Addon test",
    addons: ["ponytail"],
  });
  const warningBackup = path.join(addonSession.directory, "conn.php___");
  fs.writeFileSync(warningBackup, "private backup fixture\n");
  await page.evaluate((id) => window.workflow.selectSession(id), addonSession.id);
  const startAddon = page.locator('[data-id="' + addonSession.id + '"]')
    .getByTitle("Aynı konuşmaya devam et", { exact: true });
  await startAddon.click();
  const sharingWarning = page.getByRole("dialog", { name: "Hassas olabilecek proje dosyaları" });
  await sharingWarning.getByText("conn.php___", { exact: true }).waitFor();
  assert(!(await sharingWarning.textContent()).includes("private backup fixture"));
  await sharingWarning.getByRole("button", { name: "Vazgeç", exact: true }).click();
  await sharingWarning.waitFor({ state: "hidden" });
  assert(!(await api("app", "state")).active.some((s) => s.id === addonSession.id));
  fs.unlinkSync(warningBackup);
  const largeScanFile = path.join(addonSession.directory, "large-scan-fixture.txt");
  fs.writeFileSync(largeScanFile, Buffer.alloc(64 * 1024, "a"));
  assert.deepEqual(await api("app", "sharing_warnings", { sessionId: addonSession.id }), {
    files: [],
    truncated: true,
  });
  await startAddon.click();
  const partialWarning = page.getByRole("dialog", { name: "Dosya taraması eksik kaldı" });
  await partialWarning.getByText("Taranan kısımda hassas olabilecek dosya bulunmadı.", { exact: true }).waitFor();
  await partialWarning.getByText("Bazı dosyalar tam olarak taranamadı.", { exact: true }).waitFor();
  await partialWarning.getByRole("button", { name: "Vazgeç", exact: true }).click();
  await partialWarning.waitFor({ state: "hidden" });
  assert(!(await api("app", "state")).active.some((s) => s.id === addonSession.id));
  await assert.rejects(api("terminal", "start", { id: addonSession.id }), /doğrulanmış sürüme yükselt/);
  await assert.rejects(api("addons", "install", { id: "ponytail", revision: "old" }), /kütüphaneden onayla/);
  const ponytail = (await api("addons", "list")).find((a) => a.id === "ponytail");
  assert.equal(ponytail.installed, false);
  assert.equal(ponytail.command, null);
  assert(ponytail.files.includes("ponytail-review/SKILL.md"));
  await page.locator("#addons").click();
  await dialog.getByRole("button", { name: "Yükselt", exact: true }).click();
  await dialog.getByText("ponytail-review/SKILL.md", { exact: true }).waitFor();
  await dialog.getByRole("button", { name: "Vazgeç", exact: true }).click();
  assert(!fs.existsSync(path.join(addonRoot, "verified.json")));
  if (process.env.WORKFLOW_TEST_ADDON_DOWNLOADS === "1") {
    await api("addons", "install", { id: "ponytail", revision: ponytail.revision });
    const record = JSON.parse(fs.readFileSync(path.join(addonRoot, "verified.json"), "utf8"));
    const cached = path.join(addonRoot, record.generation, "skills/ponytail/SKILL.md");
    const original = fs.readFileSync(cached);
    fs.appendFileSync(cached, "\nTampered fixture\n");
    await assert.rejects(api("terminal", "start", { id: addonSession.id }), /bütünlük denetimi/);
    fs.writeFileSync(cached, original);
    const addonRun = await api("terminal", "start", { id: addonSession.id });
    assert(fs.existsSync(path.join(addonSession.directory, ".agents/skills/ponytail/SKILL.md")));
    await api("terminal", "stop", { id: addonSession.id, runId: addonRun.runId });
    await until(async () => !(await api("app", "state")).active.some((s) => s.id === addonSession.id), "addon stop");
    const openspec = (await api("addons", "list")).find((a) => a.id === "openspec");
    await api("addons", "install", { id: "openspec", revision: openspec.revision });
    await api("addons", "select", { id: addonSession.id, addons: ["openspec"] });
    const specRun = await api("terminal", "start", { id: addonSession.id });
    assert(fs.existsSync(path.join(addonSession.directory, ".agents/skills/openspec-propose/SKILL.md")));
    await api("terminal", "stop", { id: addonSession.id, runId: specRun.runId });
    await until(async () => !(await api("app", "state")).active.some((s) => s.id === addonSession.id), "openspec stop");
  }
  await api("addons", "select", { id: addonSession.id, addons: [] });
  await startAddon.click();
  await partialWarning.getByRole("button", { name: "Devam et", exact: true }).click();
  const restartAddon = await until(
    async () => (await api("app", "state")).active.find((s) => s.id === addonSession.id),
    "session starts after accepting an incomplete scan",
  );
  fs.unlinkSync(largeScanFile);
  assert(
    !fs.existsSync(
      path.join(addonSession.directory, ".agents/skills/openspec-propose/SKILL.md"),
    ),
  );
  await api("terminal", "stop", {
    id: addonSession.id,
    runId: restartAddon.runId,
  });
  console.log("PASS incomplete local scan with no findings warns before cancelling or continuing");
  console.log("PASS addon review, explicit revision, legacy upgrade and optional pinned download checks");
  const s = sessions[0],
    working = path.join(s.directory, "sample.txt");
  fs.writeFileSync(working, "alpha\nchanged\n");
  const changes = await api("files", "changes", { sessionId: s.id });
  assert(changes.some((f) => f.path === "sample.txt"));
  const d = await api("files", "diff", { sessionId: s.id, path: "sample.txt" });
  assert(d.patch.includes("+changed"));
  const indices = d.lines.filter((l) => l.tag !== "equal").map((l) => l.index);
  const restoreArgs = {
    sessionId: s.id,
    path: "sample.txt",
    stamp: d.stamp,
    indices,
  };
  const undoFolder = path.join(data, "undo");
  fs.writeFileSync(undoFolder, "block creation of the undo journal");
  try {
    await assert.rejects(api("files", "restore", restoreArgs));
    assert.equal(fs.readFileSync(working, "utf8"), "alpha\nchanged\n");
  } finally {
    fs.unlinkSync(undoFolder);
  }
  const undo = await api("files", "restore", restoreArgs);
  assert.equal(fs.readFileSync(working, "utf8"), "alpha\nbeta\n");
  await api("files", "undo_restore", { sessionId: s.id, token: undo.id });
  assert.equal(fs.readFileSync(working, "utf8"), "alpha\nchanged\n");
  fs.writeFileSync(path.join(project, "sample.txt"), "main edit\n");
  let transfer = await api("files", "transfer_list", { sessionId: s.id });
  assert(transfer[0].conflict);
  await assert.rejects(
    api("files", "transfer", { sessionId: s.id, files: transfer }),
  );
  const applied = await api("files", "transfer", {
    sessionId: s.id,
    files: transfer.map((f) => ({ ...f, resolution: "session" })),
  });
  assert.equal(
    fs.readFileSync(path.join(project, "sample.txt"), "utf8"),
    "alpha\nchanged\n",
  );
  await api("files", "undo_transfer", { sessionId: s.id, token: applied.id });
  assert.equal(
    fs.readFileSync(path.join(project, "sample.txt"), "utf8"),
    "main edit\n",
  );
  assert(fs.existsSync(working));
  fs.writeFileSync(path.join(s.directory, "new.txt"), "new file\n");
  transfer = await api("files", "transfer_list", { sessionId: s.id });
  const sample = path.join(project, "sample.txt");
  const selections = ["new.txt", "sample.txt"].map((file) => ({
    ...transfer.find((entry) => entry.path === file),
    resolution: "session",
  }));
  fs.chmodSync(sample, 0o444);
  try {
    await assert.rejects(api("files", "transfer", { sessionId: s.id, files: selections }));
    assert(!fs.existsSync(path.join(project, "new.txt")), "first write rolled back");
    assert.equal(fs.readFileSync(sample, "utf8"), "main edit\n");
    assert.equal((await api("files", "transfer_history", { sessionId: s.id }))[0].state, "rolled_back");
  } finally {
    fs.chmodSync(sample, 0o644);
  }
  const interrupted = await api("files", "transfer", {
    sessionId: s.id,
    files: selections,
  });
  const journal = path.join(data, "transfers", interrupted.id + ".json");
  fs.writeFileSync(journal, JSON.stringify({ ...interrupted, state: "prepared" }));
  fs.writeFileSync(path.join(project, "sample.txt"), "main edit\n");
  const recovered = await api("files", "undo_transfer", {
    sessionId: s.id,
    token: interrupted.id,
  });
  assert.equal(recovered.state, "undone");
  assert.equal(fs.readFileSync(path.join(project, "sample.txt"), "utf8"), "main edit\n");
  assert(!fs.existsSync(path.join(project, "new.txt")));
  console.log(
    "PASS Git-free baseline, failed restore journal, selected-line undo, conflict transfer and transfer rollback",
  );
  const gs = await api("app", "create_session", {
    projectId: gp.id,
    provider: "codex",
    name: "Git test",
  });
  assert.equal(gs.workspaceKind, "worktree");
  await saveText({ sessionId: gs.id, path: "a.txt" }, "new\n");
  assert.equal(fs.readFileSync(path.join(gitProject, "a.txt"), "utf8").trim(), "initial");
  let state = await api("git", "state", { sessionId: gs.id });
  await api("git", "stage", {
    sessionId: gs.id,
    stamp: state.stamp,
    files: ["a.txt"],
  });
  state = await api("git", "state", { sessionId: gs.id });
  await api("git", "commit", {
    sessionId: gs.id,
    stamp: state.stamp,
    message: "Second test",
  });
  const history = await api("git", "history", { sessionId: gs.id });
  assert(history.length >= 2);
  const cf = await api("git", "commit_files", {
    sessionId: gs.id,
    commit: history[0].hash,
    from: history[1].hash,
  });
  assert(cf.some((f) => f.path === "a.txt"));
  const cd = await api("git", "commit_diff", {
    sessionId: gs.id,
    commit: history[0].hash,
    from: history[1].hash,
    path: "a.txt",
  });
  assert(cd.patch.includes("+new"));
  const inWorktree = (args) => git(["-C", gs.directory, ...args]);
  const changesPanel = page.locator("#panel-changes");
  const diff = changesPanel.locator("#change-diff");
  await page.evaluate(async (id) => {
    await window.workflow.refresh();
    window.workflow.selectSession(id);
    await window.workflow.openPanel("changes");
  }, gs.id);
  assert.equal(await page.locator('#panel-tabs button[data-tab="git"]').count(), 0);
  await changesPanel.locator("#changes-mode-git").click();
  await changesPanel.locator("#git-working").getByText("Değişiklik yok.").waitFor();
  await changesPanel.locator("#changes-mode-session").click();
  await diff.locator(".insert .review-code").getByText("new", { exact: true }).waitFor();
  assert.match(await changesPanel.locator("#change-comparison").textContent(), /Oturum başlangıcı/);
  assert(await changesPanel.locator("#restore-file").isVisible());
  await changesPanel.locator("#changes-mode-history").click();
  await diff.locator(".insert .review-code").getByText("new", { exact: true }).waitFor();
  await changesPanel.locator("#history-from").selectOption(history[1].hash);
  await changesPanel.locator("#history-swap").click();
  await diff.locator(".delete .review-code").getByText("new", { exact: true }).waitFor();
  assert(!(await changesPanel.locator("#restore-file").isVisible()));

  fs.writeFileSync(path.join(gs.directory, "a.txt"), "staged value\n");
  inWorktree(["add", "--", "a.txt"]);
  fs.writeFileSync(path.join(gs.directory, "a.txt"), "working value  ");
  fs.mkdirSync(path.join(gs.directory, "review-draft"));
  fs.writeFileSync(path.join(gs.directory, "review-draft/draft-review.txt"), "++untracked\n");
  await changesPanel.locator("#changes-mode-git").click();
  const stagedRow = changesPanel.locator("#git-staged .file-row").filter({ hasText: "a.txt" });
  const workingRow = changesPanel.locator("#git-working .file-row").filter({ hasText: "a.txt" });
  await stagedRow.locator(".change-path").click();
  await diff.locator(".insert .review-code").getByText("staged value", { exact: true }).waitFor();
  await workingRow.locator(".change-path").click();
  await diff.locator(".review-newline").waitFor();
  assert.equal(await diff.locator(".insert mark").textContent(), "working value  ");
  assert.match(await changesPanel.locator("#change-comparison").textContent(), /Hazırlanan içerik → Çalışma alanı/);
  await changesPanel.locator("#git-working .change-path").filter({ hasText: "draft-review.txt" }).click();
  await diff.locator(".insert .review-code").getByText("++untracked", { exact: true }).waitFor();
  await changesPanel.locator("#show-commit").click();
  assert(await changesPanel.locator("#commit-message").isVisible());
  await changesPanel.locator("#show-commit").click();
  const headBeforeReview = inWorktree(["rev-parse", "HEAD"]).trim();
  await stagedRow.getByTitle("Hazırlananlardan çıkar").click();
  await changesPanel.locator("#git-staged").getByText("Henüz hazırlanmış dosya yok.").waitFor();
  await workingRow.getByTitle("Commit için hazırla").click();
  await stagedRow.locator(".change-path").waitFor();
  assert.equal(inWorktree(["rev-parse", "HEAD"]).trim(), headBeforeReview);
  inWorktree(["restore", "--source=HEAD", "--staged", "--worktree", "--", "a.txt"]);
  fs.unlinkSync(path.join(gs.directory, "review-draft/draft-review.txt"));
  fs.rmdirSync(path.join(gs.directory, "review-draft"));

  const reviewLines = Array.from({ length: 60 }, (_, i) => "context " + (i + 1));
  const reviewName = "review-context.txt";
  fs.writeFileSync(path.join(project, reviewName), reviewLines.join("\n") + "\n");
  const visualReview = await api("app", "create_session", {
    projectId: p.id, provider: "codex", name: "Diff view test",
  });
  fs.unlinkSync(path.join(project, reviewName));
  const subject = [...reviewLines];
  subject[5] = "first review edit";
  subject[49] = "second review edit";
  fs.writeFileSync(path.join(visualReview.directory, reviewName), subject.join("\n") + "\n");
  await page.evaluate(async (id) => {
    await window.workflow.refresh();
    window.workflow.selectSession(id);
    await window.workflow.openPanel("changes");
  }, visualReview.id);
  await changesPanel.locator("#changes-mode-session").waitFor();
  assert(!(await changesPanel.locator("#changes-mode-git").isVisible()));
  await changesPanel.locator("#change-list .change-path").filter({ hasText: reviewName }).click();
  await diff.locator(".review-lines .insert").first().waitFor();
  const folded = diff.locator(".review-fold").first();
  assert(!(await folded.evaluate((el) => el.open)));
  await folded.locator("summary").click();
  assert(await folded.evaluate((el) => el.open));
  await diff.getByRole("button", { name: "Sonraki değişiklik" }).click();
  assert.equal(await diff.locator(".review-position").textContent(), "1 / 2");
  await diff.getByRole("button", { name: "Sonraki değişiklik" }).click();
  assert.equal(await diff.locator(".review-position").textContent(), "2 / 2");
  await dragPanel("right", wide.width - 3);
  assert(!(await diff.getByRole("button", { name: "Yan yana" }).isVisible()));
  await page.setViewportSize({ width: 1540, height: 820 });
  await page.locator("#panel-expand").click();
  await diff.getByRole("button", { name: "Yan yana" }).click();
  assert(await diff.locator(".review-diff.split").isVisible());
  await diff.locator(".insert input").last().check();
  await changesPanel.locator("#restore-lines").click();
  await page.locator("#dialog-footer button.primary").click();
  await until(() => !fs.readFileSync(path.join(visualReview.directory, reviewName), "utf8")
    .includes("second review edit"), "selected restore from a split diff");
  assert(fs.readFileSync(path.join(visualReview.directory, reviewName), "utf8").includes("first review edit"));
  const reviewViewport = await page.context().newCDPSession(page);
  await reviewViewport.send("Emulation.clearDeviceMetricsOverride");
  await reviewViewport.detach();
  await page.evaluate(() => {
    document.documentElement.style.removeProperty("--inspector");
    localStorage.removeItem("agentloom-widths");
  });
  await api("app", "session_patch", { id: visualReview.id, archived: true });
  await page.evaluate(async (id) => {
    await window.workflow.refresh();
    window.workflow.selectSession(id);
    await window.workflow.openPanel("browser");
  }, s.id);
  console.log("PASS unified changes, Git endpoints, commit comparison, staging and Git-free review");
  inWorktree(["switch", "-c", "test/conflict-ours"]);
  for (const file of ["[ab].txt", "b.txt"]) fs.writeFileSync(path.join(gs.directory, file), file);
  state = await api("git", "state", { sessionId: gs.id });
  const staged = await api("git", "stage", {
    sessionId: gs.id, stamp: state.stamp, files: ["[ab].txt"],
  });
  assert.equal(staged.state.files.find((f) => f.path === "[ab].txt").index, "A");
  assert(staged.state.files.find((f) => f.path === "b.txt").untracked);
  fs.unlinkSync(path.join(gs.directory, "b.txt"));
  inWorktree(["mv", "a.txt", "renamed.txt"]);
  state = await api("git", "state", { sessionId: gs.id });
  assert.equal(state.files.find((f) => f.path === "renamed.txt").oldPath, "a.txt");
  const unstaged = await api("git", "stage", {
    sessionId: gs.id, stamp: state.stamp, files: ["renamed.txt"], unstage: true,
  });
  assert.equal(inWorktree(["diff", "--cached", "--name-status"]).trim(), "A\t[ab].txt");
  assert(unstaged.state.files.find((f) => f.path === "renamed.txt").untracked);
  assert.equal(unstaged.state.files.find((f) => f.path === "a.txt").work, "D");
  inWorktree(["add", "--", "a.txt", "renamed.txt"]);
  inWorktree(["commit", "-m", "Rename fixture"]);
  const rename = await api("git", "commit_files", {
    sessionId: gs.id, commit: inWorktree(["rev-parse", "HEAD"]).trim(),
  });
  assert(rename.some((f) => f.path === "a.txt" && f.status === "D"));
  assert(rename.some((f) => f.path === "renamed.txt" && f.status === "A"));
  const binary = path.join(gs.directory, "conflict.bin");
  fs.writeFileSync(binary, Buffer.from([0, 1]));
  inWorktree(["add", "conflict.bin"]);
  inWorktree(["commit", "-m", "Binary base"]);
  inWorktree(["branch", "test/conflict-theirs"]);
  const exact = Buffer.from([0, 255, 32, 13, 10, 13, 10]);
  fs.writeFileSync(binary, exact);
  inWorktree(["commit", "-am", "Binary ours"]);
  inWorktree(["switch", "test/conflict-theirs"]);
  fs.writeFileSync(binary, Buffer.from([0, 7]));
  inWorktree(["commit", "-am", "Binary theirs"]);
  inWorktree(["switch", "test/conflict-ours"]);
  const merge = cp.spawnSync("git", ["merge", "--no-edit", "test/conflict-theirs"], {
    cwd: gs.directory, windowsHide: true,
  });
  assert.equal(merge.status, 1);
  state = await api("git", "state", { sessionId: gs.id });
  assert(state.files.some((f) => f.path === "conflict.bin" && f.conflict));
  const conflict = await api("git", "conflict_details", { sessionId: gs.id, path: "conflict.bin" });
  assert.equal(conflict.text, null);
  await api("git", "conflict_resolve", {
    sessionId: gs.id, stamp: state.stamp, path: "conflict.bin", choice: "ours",
    fileStamp: conflict.hash,
  });
  assert.deepEqual(fs.readFileSync(binary), exact);
  assert.deepEqual(cp.execFileSync("git", ["show", ":conflict.bin"], { cwd: gs.directory }), exact);
  inWorktree(["merge", "--abort"]);
  inWorktree(["switch", gs.branch]);
  console.log("PASS worktree, stage, commit, branches and two-commit diff");

  const shaRepo = path.join(temporary, "sha256-project");
  git(["init", "--object-format=sha256", shaRepo]);
  const inShaRepo = (args) => git(["-C", shaRepo, ...args]);
  inShaRepo(["config", "user.name", "Workflow Test"]);
  inShaRepo(["config", "user.email", "test@localhost"]);
  const frontend = path.join(shaRepo, "frontend");
  fs.mkdirSync(frontend);
  fs.writeFileSync(path.join(shaRepo, "root.txt"), "repository root\n");
  fs.writeFileSync(path.join(frontend, "app.txt"), "initial frontend\n");
  fs.writeFileSync(path.join(frontend, "deleted.txt"), "delete in working copy\n");
  inShaRepo(["add", "--", "."]);
  inShaRepo(["-c", "core.hooksPath=", "commit", "-m", "SHA-256 root"]);
  const shaHead = inShaRepo(["rev-parse", "HEAD"]).trim();
  assert.equal(shaHead.length, 64);
  fs.writeFileSync(path.join(frontend, "app.txt"), "dirty frontend\n");
  fs.unlinkSync(path.join(frontend, "deleted.txt"));
  const nestedProject = await api("app", "add_project", { directory: frontend });
  const nested = await api("app", "create_session", {
    projectId: nestedProject.id,
    provider: "codex",
    name: "Nested Git test",
  });
  assert.equal(nested.workspaceKind, "worktree");
  assert.equal(path.basename(nested.directory), "frontend");
  assert.equal(fs.readFileSync(path.join(nested.directory, "app.txt"), "utf8"), "dirty frontend\n");
  assert(!fs.existsSync(path.join(nested.directory, "deleted.txt")));
  assert(!fs.existsSync(path.join(nested.directory, "root.txt")));
  assert.equal(fs.readFileSync(path.join(nested.directory, "..", "root.txt"), "utf8").trim(), "repository root");
  const nestedContext = { sessionId: nested.id };
  const rootFiles = await api("git", "commit_files", { ...nestedContext, commit: shaHead });
  assert.deepEqual(rootFiles, [
    { path: "frontend/app.txt", status: "A" },
    { path: "frontend/deleted.txt", status: "A" },
    { path: "root.txt", status: "A" },
  ]);
  const rootDiff = await api("git", "commit_diff", {
    ...nestedContext, commit: shaHead, path: "frontend/app.txt",
  });
  assert(rootDiff.patch.includes("+initial frontend"));
  const nestedState = await api("git", "state", nestedContext);
  assert.deepEqual(nestedState.files.map((file) => file.path), ["frontend/app.txt", "frontend/deleted.txt"]);
  const stagedNested = await api("git", "stage", {
    ...nestedContext, stamp: nestedState.stamp, files: ["frontend/app.txt"],
  });
  const inNested = (args) => git(["-C", nested.directory, ...args]);
  inNested(["switch", "-c", "test/externally-switched"]);
  const externallySwitched = await api("git", "state", nestedContext);
  assert.equal(externallySwitched.head, stagedNested.state.head);
  assert.deepEqual(externallySwitched.files, stagedNested.state.files);
  assert.notEqual(externallySwitched.stamp, stagedNested.state.stamp);
  for (const action of ["commit", "push"]) {
    await assert.rejects(api("git", action, {
      ...nestedContext, stamp: stagedNested.state.stamp, message: "Wrong branch",
    }), /Git durumu değişti/);
  }
  assert.equal(inNested(["rev-parse", "HEAD"]).trim(), shaHead);
  assert.equal(externallySwitched.files[0].index, "M");
  console.log("PASS nested worktree layout, stale commit/push rejection and SHA-256 root diff");

  const originalBranch = git(["branch", "--show-current"]).trim();
  git(["switch", "-c", "feature/direct-mode"]);
  const worktrees = git(["worktree", "list", "--porcelain"]);
  await page.evaluate((id) => window.workflow.selectProject(id), gp.id);
  await page.locator("#new-session").click();
  const newSession = page.locator("#dialog");
  const workspaceChoice = newSession.getByLabel("Çalışma alanı", { exact: true });
  assert.equal(await workspaceChoice.inputValue(), "isolated");
  await workspaceChoice.selectOption("direct");
  await newSession.getByText(/Seçili dal feature\/direct-mode/).waitFor();
  await newSession.getByLabel("Oturum adı").fill("Direct folder test");
  await newSession.getByRole("button", { name: "Oturumu aç" }).click();
  await newSession.waitFor({ state: "hidden" });
  const direct = (await api("app", "state")).sessions.find(
    (item) => item.name === "Direct folder test",
  );
  assert.equal(direct.workspaceKind, "direct");
  assert.equal(fs.realpathSync(direct.directory), fs.realpathSync(gitProject));
  assert.equal(direct.branch, "feature/direct-mode");
  assert.equal(git(["worktree", "list", "--porcelain"]), worktrees);
  const directRun = await api("terminal", "start", { id: direct.id });
  pids.push(directRun.pid);
  await api("terminal", "write", {
    id: direct.id,
    runId: directRun.runId,
    data: "write-workspace-marker\r",
  });
  const directMarker = "direct-session-marker.txt";
  await until(() => fs.existsSync(path.join(gitProject, directMarker)), "direct terminal writes into project");
  assert.equal(fs.existsSync(path.join(gs.directory, directMarker)), false);
  assert((await api("files", "changes", { sessionId: direct.id })).some(
    (item) => item.path === directMarker && item.status === "A",
  ));
  await assert.rejects(api("files", "transfer_list", { sessionId: direct.id }), /zaten proje klasöründe/);
  await page.evaluate(() => window.workflow.openPanel("changes"));
  assert.equal(await page.locator("#transfer-open").isVisible(), false);
  assert.deepEqual(await api("history", "discover", { id: direct.id }), { found: false });
  let directGit = await api("git", "state", { sessionId: direct.id });
  assert.equal(directGit.branch, "feature/direct-mode");
  await api("git", "stage", {
    sessionId: direct.id,
    stamp: directGit.stamp,
    files: [directMarker],
  });
  directGit = await api("git", "state", { sessionId: direct.id });
  await api("git", "commit", {
    sessionId: direct.id,
    stamp: directGit.stamp,
    message: "Direct folder test commit",
  });
  assert.equal(git(["log", "-1", "--format=%s"]).trim(), "Direct folder test commit");
  assert.equal(git(["log", "-1", "--format=%s", gs.branch]).trim(), "Second test");
  const directFile = { sessionId: direct.id, path: "a.txt" };
  await saveText(directFile, "Doğrudan klasör\n");
  assert.equal(fs.readFileSync(path.join(gitProject, "a.txt"), "utf8").trim(), "Doğrudan klasör");
  await saveText(directFile, "initial\n");
  const { revision: directRevision } = await api("files", "edit_read", directFile);
  git(["switch", originalBranch]);
  await assert.rejects(api("files", "edit_save", {
    ...directFile, revision: directRevision, text: "wrong branch\n",
  }), /seçili Git dalı değişti/);
  assert.equal((await api("git", "state", { sessionId: direct.id })).branch, originalBranch);
  await assert.rejects(api("files", "restore", {
    sessionId: direct.id, path: directMarker, stamp: null,
  }), /oturumun açıldığı Git dalına dön/);
  const conversationLogs = path.join(data, "test-codex-home", "sessions");
  fs.mkdirSync(conversationLogs, { recursive: true });
  for (const id of ["conversation-a", "conversation-b"]) {
    fs.writeFileSync(path.join(conversationLogs, id + ".jsonl"), JSON.stringify({
      type: "session_meta", payload: { id, cwd: direct.directory, timestamp: new Date().toISOString() },
    }) + "\n");
  }
  const bindDirect = (conversationId) => api("history", "bind", { id: direct.id, conversationId });
  await bindDirect("conversation-a");
  await assert.rejects(bindDirect("conversation-b"), /önce oturumu durdur/);
  const bound = (await api("app", "state")).sessions.find((item) => item.id === direct.id);
  assert.equal(bound.conversationId, "conversation-a");
  assert.equal(bound.runId, directRun.runId);
  const linkArgs = { source: gs.id, target: direct.id, read: true, files: true, work: true };
  const outgoing = await api("app", "link", linkArgs);
  const incoming = await api("app", "link", { source: direct.id, target: gs.id, read: true, files: true, work: true });
  await api("app", "session_patch", { id: gs.id, position: { x: 40, y: 50 } });
  await api("app", "session_patch", { id: direct.id, position: { x: 40, y: 630 } });
  await page.locator("#view-map").click();
  await page.locator("#fit").click();
  const outgoingBadge = page.locator(`[data-link-id="${outgoing.id}"]`);
  const incomingBadge = page.locator(`[data-link-id="${incoming.id}"]`);
  await outgoingBadge.waitFor({ state: "visible" });
  await incomingBadge.waitFor({ state: "visible" });
  const linkTooltip = page.locator(".link-tooltip");
  await linkTooltip.waitFor({ state: "hidden" });
  await outgoingBadge.hover();
  await linkTooltip.getByText("Konuşma · Dosya · İş", { exact: true }).waitFor();
  await page.locator("#fit").hover();
  await linkTooltip.waitFor({ state: "hidden" });
  await outgoingBadge.click();
  await dialog.getByText("Oturum bağlantısı").waitFor();
  await dialog.getByRole("button", { name: "Vazgeç" }).click();
  await dialog.waitFor({ state: "hidden" });
  async function assertSeparateBadges() {
    const one = await outgoingBadge.boundingBox(), two = await incomingBadge.boundingBox();
    assert(one.x + one.width <= two.x || two.x + two.width <= one.x ||
      one.y + one.height <= two.y || two.y + two.height <= one.y, "reciprocal permission labels have separate hit areas");
  }
  await assertSeparateBadges();
  assert(!(await outgoingBadge.getAttribute("aria-label")).includes("Yeniden onay"));
  const mapLinkRun = await api("terminal", "start", { id: gs.id });
  pids.push(mapLinkRun.pid);
  const readLinkedFile = () => mcp(gs.id, "read_files", { target: direct.id, paths: ["a.txt"] });
  assert((await readLinkedFile()).result?.[0]?.content?.text);
  await api("terminal", "stop", { id: direct.id, runId: directRun.runId });
  await until(async () => !(await api("app", "state")).active.some((item) => item.id === direct.id), "direct session stop");
  assert.equal((await bindDirect("conversation-b")).conversationId, "conversation-b");
  const staleCatalog = await mcp(gs.id, "session_catalog");
  assert.equal(staleCatalog.result[0].needsReapproval, true);
  assert.match((await readLinkedFile()).error, /bağlantı izinlerini yeniden kaydet/);
  await outgoingBadge.locator(".link-warning-mark").waitFor();
  await incomingBadge.locator(".link-warning-mark").waitFor();
  while (parseInt(await page.locator("#zoom").textContent(), 10) > 40) {
    await page.locator("#zoom-out").click();
  }
  await assertSeparateBadges();
  await changeLanguage("en");
  await outgoingBadge.locator(".link-warning-mark").waitFor();
  await outgoingBadge.hover();
  await linkTooltip.getByText("Conversation · Files · Work", { exact: true }).waitFor();
  await linkTooltip.getByText("Reapproval required", { exact: true }).waitFor();
  assert.match(await outgoingBadge.getAttribute("aria-label"), /Reapproval required/);
  await page.keyboard.press("Escape");
  await linkTooltip.waitFor({ state: "hidden" });
  await assertSeparateBadges();
  await changeLanguage("tr");
  await outgoingBadge.locator(".link-warning-mark").waitFor();
  await outgoingBadge.press("Enter");
  await dialog.getByText(/bir oturumun konuşması değişti/).waitFor();
  await dialog.getByRole("button", { name: "Yeniden onayla" }).click();
  await dialog.waitFor({ state: "hidden" });
  const newLink = (await api("app", "state")).links.find((link) => link.source === gs.id && link.target === direct.id);
  assert.equal(newLink.needsReapproval, false);
  await until(async () => await page.evaluate(() => document.activeElement?.dataset.linkId) === newLink.id, "focus returns to the reapproved map link");
  assert.equal((await mcp(gs.id, "session_catalog")).result[0].needsReapproval, false);
  assert((await readLinkedFile()).result?.[0]?.content?.text);
  await incomingBadge.locator(".link-warning-mark").waitFor();
  await api("app", "unlink", { id: newLink.id });
  await api("app", "unlink", { id: incoming.id });
  await api("terminal", "stop", { id: gs.id, runId: mapLinkRun.runId });
  console.log("PASS directional map permissions, reapproval marker, MCP rejection and reapproval");
  console.log("PASS direct session UI, terminal, commit, checkout safety and conversation binding");

  await page.evaluate((id) => window.workflow.selectSession(id), s.id);
  await page.evaluate(() => window.workflow.openPanel("browser"));
  const previewPort = await freePort();
  const previewUrl = "http://127.0.0.1:" + previewPort + "/";
  const previewScript = path.join(s.directory, "workflow-preview-test.cjs");
  fs.writeFileSync(
    previewScript,
    'const fs = require("node:fs");' +
      'const marker = fs.readFileSync("workflow-preview-marker.txt", "utf8");' +
      'require("node:http").createServer((req,res) => res.end(marker)).listen(' +
      'Number(process.env.WORKFLOW_PREVIEW_PORT), "127.0.0.1");',
  );
  fs.writeFileSync(
    path.join(s.directory, "workflow-preview-marker.txt"),
    "Session workspace preview",
  );
  const previewCommand =
    "& '" +
    process.execPath.replaceAll("'", "''") +
    "' .\\workflow-preview-test.cjs";
  await page.evaluate((id) => window.workflow.selectSession(id), s.id);
  await page.locator("#view-focus").click();
  const previewButton = page.locator(
    '.session-card[data-id="' + s.id + '"] .preview-button',
  );
  await previewButton.click();
  const previewModal = page.locator("#dialog");
  await previewModal.getByLabel("Önizleme komutu").fill(previewCommand);
  await previewModal.getByLabel("Yerel adres").fill(address);
  await previewModal.getByRole("button", { name: "Önizlemeyi başlat" }).click();
  await previewModal
    .locator(".dialog-error")
    .filter({ hasText: /Port .* dolu/ })
    .waitFor();
  assert.equal(await api("preview", "status", { sessionId: s.id }), null);
  await previewModal.getByLabel("Yerel adres").fill(previewUrl);
  await previewModal.getByRole("button", { name: "Önizlemeyi başlat" }).click();
  const preview = await until(
    async () => {
      const run = await api("preview", "status", { sessionId: s.id });
      if (run?.state === "error") throw Error(run.message + "\n" + run.log);
      return run?.state === "ready" && run.pageId ? run : null;
    },
    "preview ready",
    15000,
  );
  await until(async () => {
    const read = await api("browser", "read", {
      sessionId: s.id,
      pageId: preview.pageId,
    });
    return read.text.includes("Session workspace preview");
  }, "preview belongs to session workspace");
  await until(
    async () => (await page.locator("#address").inputValue()) === previewUrl,
    "preview shown in browser panel",
  );
  await previewButton.click();
  await previewModal.getByRole("button", { name: "Önizlemeyi durdur" }).click();
  assert.equal(
    (await api("preview", "status", { sessionId: s.id })).state,
    "stopped",
  );
  await until(async () => {
    try {
      await fetch(previewUrl);
      return false;
    } catch {
      return true;
    }
  }, "preview process and server stopped");
  await api("browser", "close", { sessionId: s.id, pageId: preview.pageId });
  console.log(
    "PASS session preview starts in its own workspace, opens its browser and protects occupied ports",
  );
  const browserPage = await api("browser", "open", {
    sessionId: s.id,
    url: address,
    newTab: true,
  });
  await until(async () => {
    const r = await api("browser", "read", {
      sessionId: s.id,
      pageId: browserPage.id,
    });
    return r.elements.some((e) => e.text === "Save");
  }, "embedded page");
  const read = await api("browser", "read", {
    sessionId: s.id,
    pageId: browserPage.id,
    screenshot: true,
  });
  assert(read.screenshot.startsWith("data:image/png;base64,"));
  const name = read.elements.find(
      (e) => e.tag === "input" && e.type === "text",
    ),
    save = read.elements.find((e) => e.text === "Save");
  await api("browser", "action", {
    sessionId: s.id,
    pageId: browserPage.id,
    action: "fill",
    ref: name.ref,
    documentId: read.documentId,
    text: "Native Rust",
  });
  await api("browser", "action", {
    sessionId: s.id,
    pageId: browserPage.id,
    action: "click",
    ref: save.ref,
    documentId: read.documentId,
  });
  let reread = await api("browser", "read", {
    sessionId: s.id,
    pageId: browserPage.id,
  });
  assert(reread.text.includes("Native Rust"));
  await assert.rejects(
    api("browser", "action", {
      sessionId: s.id,
      pageId: browserPage.id,
      action: "click",
      ref: save.ref,
      documentId: "stale",
    }),
  );
  const pickButton = page.locator("#browser-pick");
  await pickButton.click();
  await until(
    async () => (await pickButton.getAttribute("aria-pressed")) === "true",
    "picker button active",
  );
  await pickButton.click();
  await until(
    async () => (await pickButton.getAttribute("aria-pressed")) === "false",
    "picker button canceled",
    10000,
  );
  assert.equal(
    (await api("browser", "poll", { sessionId: s.id, pageId: browserPage.id }))
      .picking,
    false,
  );
  await pickButton.click();
  await until(
    async () => (await pickButton.getAttribute("aria-pressed")) === "true",
    "picker active for main Escape",
  );
  await page.keyboard.press("Escape");
  await until(
    async () =>
      !(
        await api("browser", "poll", {
          sessionId: s.id,
          pageId: browserPage.id,
        })
      ).picking,
    "main Escape canceled",
  );
  await api("browser", "pick", { sessionId: s.id, pageId: browserPage.id });
  await api("browser", "action", {
    sessionId: s.id,
    pageId: browserPage.id,
    action: "key",
    key: "Escape",
  });
  await until(
    async () =>
      !(
        await api("browser", "poll", {
          sessionId: s.id,
          pageId: browserPage.id,
        })
      ).picking,
    "native Escape canceled",
  );
  await api("browser", "pick", { sessionId: s.id, pageId: browserPage.id });
  await api("browser", "action", {
    sessionId: s.id,
    pageId: browserPage.id,
    action: "click",
    x: save.bounds.x + save.bounds.width / 2,
    y: save.bounds.y + save.bounds.height / 2,
  });
  const concurrentPicks = await Promise.all(
    Array.from({ length: 3 }, () =>
      api("browser", "poll", { sessionId: s.id, pageId: browserPage.id }),
    ),
  );
  assert(
    new Set(concurrentPicks.filter((p) => p.picked).map((p) => p.picked.id))
      .size <= 1,
    "parallel polls preserve the same selection identity",
  );
  const picked = await until(async () => {
    const r = await api("browser", "poll", {
      sessionId: s.id,
      pageId: browserPage.id,
    });
    return r.picked ? r : null;
  }, "native element selection");
  assert(picked.picked.html.includes("button"));
  assert.equal(picked.picking, false);
  const afterPickRead = await api("browser", "read", {
    sessionId: s.id,
    pageId: browserPage.id,
  });
  const afterPickName = afterPickRead.elements.find(
      (e) => e.tag === "input" && e.type === "text",
    ),
    afterPickSave = afterPickRead.elements.find((e) => e.text === "Save");
  await api("browser", "action", {
    sessionId: s.id,
    pageId: browserPage.id,
    action: "fill",
    ref: afterPickName.ref,
    documentId: afterPickRead.documentId,
    text: "After picker",
  });
  await api("browser", "action", {
    sessionId: s.id,
    pageId: browserPage.id,
    action: "click",
    ref: afterPickSave.ref,
    documentId: afterPickRead.documentId,
  });
  assert(
    (
      await api("browser", "read", { sessionId: s.id, pageId: browserPage.id })
    ).text.includes("After picker"),
    "native page receives ordinary clicks after selection",
  );
  await api("browser", "pick", { sessionId: s.id, pageId: browserPage.id });
  await api("browser", "cancel_pick", {
    sessionId: s.id,
    pageId: browserPage.id,
  });
  assert.equal(
    (await api("browser", "poll", { sessionId: s.id, pageId: browserPage.id }))
      .picked.id,
    picked.picked.id,
    "cancel keeps the prior selection",
  );
  await api("browser", "clear_pick", {
    sessionId: s.id,
    pageId: browserPage.id,
    pickId: picked.picked.id,
  });
  const cleared = await api("browser", "poll", {
    sessionId: s.id,
    pageId: browserPage.id,
  });
  assert.equal(cleared.picked, null);
  console.log(
    "PASS embedded WebView2 CDP, screenshot, fill, click, native element picker and clear",
  );
  const second = await api("browser", "open", {
    sessionId: s.id,
    url: address + "/api",
    newTab: true,
  });
  await until(async () => {
    const r = await api("browser", "read", {
      sessionId: s.id,
      pageId: second.id,
    });
    return r.json?.ready;
  }, "JSON page");
  assert.equal(
    (await api("browser", "tabs", { sessionId: s.id })).tabs.length,
    2,
  );
  await api("browser", "close", { sessionId: s.id, pageId: second.id });
  await api("app", "link", {
    source: s.id,
    target: sessions[1].id,
    read: true,
    files: true,
    work: true,
  });
  await stdioMcp(s.id);
  await stdioMcp(s.id, true);
  const catalog = await mcp(s.id, "session_catalog");
  assert.equal(catalog.result.length, 1);
  const context = await mcp(s.id, "read_files", {
    target: sessions[1].id,
    paths: ["sample.txt"],
  });
  assert(context.result[0].content.text.includes("beta"));
  const shareRoot = sessions[1].directory;
  const credentialFixture = randomBytes(16).toString("hex");
  fs.writeFileSync(path.join(project, ".workflowignore"), "/private-note.txt\n");
  fs.writeFileSync(path.join(shareRoot, "private-note.txt"), "ignored fixture\n");
  fs.writeFileSync(path.join(shareRoot, "conn.php___"), "backup fixture\n");
  fs.writeFileSync(path.join(shareRoot, "sample.conf"), "$password = '" + credentialFixture + "';\n");
  const warning = await api("app", "sharing_warnings", { sessionId: sessions[1].id });
  assert.deepEqual(warning.files, ["conn.php___", "sample.conf"]);
  assert(!JSON.stringify(warning).includes(credentialFixture));
  for (const name of ["private-note.txt", "conn.php___", "sample.conf"]) {
    const result = await mcp(s.id, "read_files", { target: sessions[1].id, paths: ["sample.txt", name] });
    assert(result.error && !result.result);
    assert(!JSON.stringify(result).includes(credentialFixture));
  }
  const visibleChanges = await mcp(s.id, "read_files", { target: sessions[1].id });
  assert(!visibleChanges.result.some((change) => ["private-note.txt", "conn.php___", "sample.conf"].includes(change.path)));
  for (const name of ["private-note.txt", "conn.php___", "sample.conf"])
    fs.unlinkSync(path.join(shareRoot, name));
  fs.unlinkSync(path.join(project, ".workflowignore"));
  console.log("PASS MCP denies protected paths and credential content, and local scan returns filenames only");
  const denied = await mcp(s.id, "read_context", { target: sessions[2].id });
  assert(denied.error);
  const bad = await mcp(s.id, "session_catalog", {}, { token: randomBytes(24).toString("hex") });
  assert(bad.closed);
  const job = await mcp(s.id, "request_work", {
    target: sessions[1].id,
    instruction: "Review the example",
  });
  assert.equal(job.result.state, "pending");
  const targetId = sessions[1].id;
  await page.evaluate(async (id) => {
    await window.workflow.refresh();
    window.workflow.selectSession(id);
    await window.workflow.openPanel("links");
  }, targetId);
  const targetInput = page.locator(`[data-id="${targetId}"] .xterm-helper-textarea`);
  const acceptJob = page.getByRole("button", { name: "Kabul et · taslağa ekle", exact: true });
  await targetInput.pressSequentially("Existing draft");
  await until(() => page.evaluate((id) => {
    const view = window.workflow.views.get(id);
    return view.inputDraft === "Existing draft" && !view.hasPendingInput();
  }, targetId), "queued terminal draft written");
  await acceptJob.click();
  await page.locator("#toast").getByText(/giriş taslağını temizle/).waitFor();
  assert.equal((await api("app", "state")).jobs.find((entry) => entry.id === job.result.id).state, "pending");
  assert.equal(await page.evaluate((id) => window.workflow.views.get(id).inputDraft, targetId), "Existing draft");
  await assert.rejects(api("app", "job", {
    id: job.result.id, decision: "accept", runId: "stale-run",
  }), /Oturum çalıştırması değişti/);
  await targetInput.press("Enter");
  await until(() => page.evaluate((id) => {
    const view = window.workflow.views.get(id);
    return !view.inputDraft && !view.hasPendingInput();
  }, targetId), "terminal draft submitted before accepting work");
  await acceptJob.click();
  await until(() => page.evaluate(
    (id) => window.workflow.views.get(id).inputDraft.includes("Review the example"), targetId,
  ), "accepted request inserted as a terminal draft");
  assert.equal((await api("app", "state")).jobs.find((entry) => entry.id === job.result.id).state, "accepted");
  const targetTranscript = path.join(data, "test-input", "claude.txt");
  assert(!fs.readFileSync(targetTranscript, "utf8").includes("Review the example"));
  await targetInput.press("Enter");
  await until(() => fs.readFileSync(targetTranscript, "utf8").includes("Review the example"), "submitted work request");
  const reported = await mcp(sessions[1].id, "report_result", {
    requestId: job.result.id,
    result: "Reviewed",
  });
  assert.equal(reported.result.state, "completed");
  const result = await mcp(s.id, "read_result", { requestId: job.result.id });
  assert.equal(result.result.result, "Reviewed");
  await page.evaluate(async (id) => {
    window.workflow.selectSession(id);
    await window.workflow.openPanel("browser");
  }, s.id);
  await api("app", "session_patch", { id: s.id, browserAccess: false });
  assert((await mcp(s.id, "browser_read", { pageId: browserPage.id })).error);
  await api("app", "session_patch", { id: s.id, browserAccess: true });
  const openedByAgent = await mcp(s.id, "browser_open", {
    url: address + "?trace=private-query",
  });
  assert(openedByAgent.result.id);
  const tracedPage = openedByAgent.result.id;
  const info = await until(async () => {
    const r = await mcp(s.id, "browser_read", { pageId: tracedPage });
    return r.result?.elements.some((e) => e.text === "Save") ? r.result : null;
  }, "agent browser page loaded");
  const traceInput = info.elements.find(
    (e) => e.tag === "input" && e.type === "text",
  );
  const traceSave = info.elements.find((e) => e.text === "Save");
  assert(
    (
      await mcp(s.id, "browser_action", {
        pageId: tracedPage,
        action: "fill",
        ref: traceInput.ref,
        documentId: info.documentId,
        text: "Trace secret input",
      })
    ).result.done,
  );
  assert(
    (
      await mcp(s.id, "browser_action", {
        pageId: tracedPage,
        action: "click",
        ref: traceSave.ref,
        documentId: info.documentId,
      })
    ).result.done,
  );
  assert.equal(
    (
      await mcp(s.id, "browser_assert", {
        pageId: tracedPage,
        check: "text_contains",
        expected: "Trace secret input",
        label: "Saved name is visible",
      })
    ).result.passed,
    true,
  );
  await page.evaluate(async (sid) => {
    window.workflow.selectSession(sid);
    await window.workflow.openPanel("browser");
  }, s.id);
  assert.equal(
    (
      await mcp(s.id, "browser_assert", {
        pageId: tracedPage,
        check: "text_contains",
        expected: "This text is absent",
        label: "Missing text",
        timeoutMs: 0,
      })
    ).result.passed,
    false,
  );
  const steps = await api("browser", "trace", { sessionId: s.id });
  assert.deepEqual(
    steps.map((step) => step.operation),
    ["open", "fill", "click", "text_contains", "text_contains"],
  );
  assert.equal(steps[2].detail, "Save");
  assert.equal(steps[4].hasImage, true);
  assert(!JSON.stringify(steps).includes("Trace secret input"));
  assert(!JSON.stringify(steps).includes("private-query"));
  await page.locator("#browser-trace").click();
  const timeline = page.locator("#dialog .trace-list");
  await timeline.getByText("✓ Doğrulandı", { exact: true }).waitFor();
  await timeline.getByText("× Başarısız", { exact: true }).waitFor();
  await timeline.getByRole("button", { name: "Hata ekranını göster" }).click();
  await timeline.locator("img").waitFor({ state: "visible" });
  await page.locator("#dialog-close").click();
  fs.writeFileSync(path.join(s.directory, "summary-test.txt"), "Oturum farkı\n");
  await page.locator("#view-map").click();
  await page.evaluate((sid) => window.workflow.selectSession(sid, true), s.id);
  await page.locator(`[data-id="${s.id}"] .card-summary`).click();
  const summary = page.locator("#panel-summary");
  await page.locator("#edge-lines .edge.edge-focused").waitFor({ state: "attached" });
  await summary.locator("#summary-links button").first().waitFor();
  await summary
    .getByText("1 başarılı · 1 başarısız · 5 tarayıcı adımı", { exact: true })
    .waitFor();
  await summary.getByRole("button", { name: /summary-test\.txt/ }).click();
  await summary.locator("#summary-diff .insert")
    .getByText("Oturum farkı", { exact: true }).waitFor();
  await summary.locator("#summary-trace")
    .getByRole("button", { name: "Hata ekranını göster" }).click();
  await summary.locator("#summary-trace img").waitFor();
  await api("browser", "close", { sessionId: s.id, pageId: tracedPage });
  console.log(
    "PASS agent browser timeline records actions, assertions and failure screenshot",
  );
  console.log(
    "PASS source-owned MCP access, links, task results, invalid token and browser revocation",
  );
  if (process.env.WORKFLOW_TEST_CHROME !== "0") {
    const chrome = await api("browser", "open", {
      sessionId: s.id,
      url: address,
      mode: "chrome",
      newTab: true,
    });
    const cr = await until(async () => {
      const r = await api("browser", "read", {
        sessionId: s.id,
        pageId: chrome.id,
      });
      return r.elements.length ? r : null;
    }, "Chrome");
    assert(cr.elements.some((e) => e.text === "Save"));
    const chromeName = cr.elements.find((e) => e.tag === "input" && e.type === "text");
    const chromeNote = cr.elements.find((e) => e.text === "Note");
    const target = { sessionId: s.id, pageId: chrome.id, documentId: cr.documentId };
    await api("browser", "action", { ...target, action: "click", ref: chromeName.ref });
    await api("browser", "action", { ...target, action: "type", ref: chromeNote.ref, text: "Targeted" });
    const focused = await api("browser", "read", { sessionId: s.id, pageId: chrome.id });
    assert.equal(focused.elements.find((e) => e.text === "Note").value, "Targeted");
    assert.equal(focused.elements.find((e) => e.tag === "input" && e.type === "text").value, "");
    await api("browser", "pick", { sessionId: s.id, pageId: chrome.id });
    const chromePort = fs
      .readFileSync(
        path.join(data, "chrome-profiles", s.id, "DevToolsActivePort"),
        "utf8",
      )
      .split(/\r?\n/)[0];
    const chromeDebug = await playwright.chromium.connectOverCDP(
      "http://127.0.0.1:" + chromePort,
    );
    const cp = chromeDebug
      .contexts()
      .flatMap((c) => c.pages())
      .find((p) => p.url().startsWith(address));
    await cp.locator("#save").click();
    const picked = await until(async () => {
      const r = await api("browser", "poll", {
        sessionId: s.id,
        pageId: chrome.id,
      });
      return r.picked ? r : null;
    }, "Chrome picker");
    await api("browser", "pick", { sessionId: s.id, pageId: chrome.id });
    await cp.keyboard.press("Escape");
    await until(
      async () =>
        !(await api("browser", "poll", { sessionId: s.id, pageId: chrome.id }))
          .picking,
      "Chrome Escape canceled",
    );
    await api("browser", "pick", { sessionId: s.id, pageId: chrome.id });
    const canceledChrome = await api("browser", "cancel_pick", {
      sessionId: s.id,
      pageId: chrome.id,
    });
    assert.equal(canceledChrome.picking, false);
    assert.equal(canceledChrome.picked.id, picked.picked.id);
    await cp.locator("#name").fill("After Chrome picker");
    await cp.locator("#save").click();
    assert.equal(
      await cp.locator("#result").textContent(),
      "After Chrome picker",
    );
    await api("browser", "clear_pick", {
      sessionId: s.id,
      pageId: chrome.id,
      pickId: picked.picked.id,
    });
    await api("browser", "close", { sessionId: s.id, pageId: chrome.id });
    await chromeDebug
      .newBrowserCDPSession()
      .then((c) => c.send("Browser.close"))
      .catch(() => {});
    await chromeDebug.close();
    console.log("PASS installed Chrome, page reading, picker and clear");
  }
  if (!(await page.locator("#provider-cards").isVisible()))
    await page.locator("#show-providers").click();
  fs.writeFileSync(path.join(usageRoot, "auth-status.json"), JSON.stringify(usageAuth));
  const addedAccounts = {};
  for (const [provider, defaultQuota, managedQuota] of [
    ["codex", 75, 60],
    ["claude", 85, 70],
    ["copilot", 40, 70],
  ]) {
    const card = page.locator(`[data-provider="${provider}"]`);
    const accountName = "İş " + provider;
    await card.getByRole("button", { name: "Hesaplar", exact: true }).click();
    await dialog.getByPlaceholder("Örn. İş").fill(accountName);
    await dialog.getByRole("button", { name: "Hesap ekle", exact: true }).click();
    await dialog.getByText("Sağlayıcının giriş adımlarını takip et.").waitFor();
    const signingIn = (await api("app", "providers")).find(p => p.id === provider).accounts.find(a => a.name === accountName);
    await assert.rejects(api("app", "provider_account_remove", { provider, accountId: signingIn.id }), /giriş işlemini tamamla veya kapat/);
    await dialog.locator(".xterm-helper-textarea").focus();
    await page.keyboard.type("connect");
    await page.keyboard.press("Enter");
    await dialog.getByText("Hesap bağlı.", { exact: true }).waitFor();
    await dialog.locator("#dialog-footer button").click();
    const { accounts } = (await api("app", "providers")).find((p) => p.id === provider);
    assert.equal(accounts[0].id, "default");
    const account = accounts.find((a) => a.name === accountName);
    assert(account?.verified);
    addedAccounts[provider] = account.id;
    const quota = async (accountId) =>
      (await api("app", "provider_usage", { provider, accountId })).windows[0].remainingPercent;
    assert.equal(await quota("default"), defaultQuota);
    assert.equal(await quota(account.id), managedQuota);
    await card.locator(`[data-account-id="${account.id}"] .usage-window`).first().waitFor();
    assert.equal(
      await card.locator(`[data-account-id="${account.id}"] [aria-valuenow]`).first().getAttribute("aria-valuenow"),
      String(managedQuota),
    );
    let session;
    if (provider === "claude") {
      await page.evaluate((id) => window.workflow.selectProject(id), p.id);
      await page.locator("#new-session").click();
      await dialog.getByRole("button", { name: /Claude Code/ }).click();
      const choice = dialog.getByLabel("Hesap", { exact: true });
      assert.equal(await choice.inputValue(), "default");
      await choice.selectOption(account.id);
      await dialog.getByLabel("Oturum adı").fill(accountName);
      await dialog.getByRole("button", { name: "Oturumu aç" }).click();
      await dialog.waitFor({ state: "hidden" });
      session = (await api("app", "state")).sessions.find((s) => s.name === accountName);
      assert.equal(session.accountId, account.id);
    } else {
      session = await api("app", "create_session", {
        projectId: p.id, provider, accountId: account.id, name: accountName,
      });
    }
    const run = await api("terminal", "start", { id: session.id });
    pids.push(run.pid);
    await until(async () => {
      const attached = await api("terminal", "attach", { id: session.id });
      return Buffer.from(attached.data, "base64").toString().includes("Account: " + account.id);
    }, provider + " selected account PTY");
    await assert.rejects(api("app", "provider_account_remove", { provider, accountId: account.id }), /çalışan oturumları durdur/);
    await assert.rejects(api("app", "provider_account_remove", { provider, accountId: "default" }), /Varsayılan hesap/);
    await card.getByRole("button", { name: "Hesaplar", exact: true }).click();
    assert(await dialog.locator(`[data-account-id="${account.id}"]`).getByRole("button", { name: "Kaldır", exact: true }).isDisabled());
    assert.equal(await dialog.locator('[data-account-id="default"]').getByRole("button", { name: "Kaldır", exact: true }).count(), 0);
    await dialog.locator("#dialog-footer button").click();
    await api("terminal", "stop", { id: session.id, runId: run.runId });
    await until(async () => !(await api("app", "state")).active.some(a => a.id === session.id), "account session stopped");
    const variable = { codex: "CODEX_HOME", claude: "CLAUDE_CONFIG_DIR", copilot: "COPILOT_HOME" }[provider];
    const accountDirectory = path.join(data, "accounts", variable, account.id);
    const conversationId = require("crypto").randomUUID();
    const logDirectory = path.join(accountDirectory, { codex: "sessions", claude: "projects", copilot: "session-state" }[provider]);
    fs.mkdirSync(logDirectory, { recursive: true });
    const metadata = provider === "codex"
      ? { type: "session_meta", payload: { id: conversationId, cwd: session.directory, timestamp: new Date().toISOString() } }
      : provider === "claude" ? { sessionId: conversationId, cwd: session.directory }
      : { type: "session.start", data: { sessionId: conversationId, cwd: session.directory } };
    const historyMessage = provider === "copilot" ? { type: "user.message", data: { content: "Synthetic preserved history" } } : { role: "user", content: "Synthetic preserved history" };
    fs.writeFileSync(path.join(logDirectory, conversationId + ".jsonl"), [metadata, historyMessage].map(v => JSON.stringify(v)).join("\n") + "\n");
    await api("history", "bind", { id: session.id, conversationId });
    const retainedSession = (await api("app", "state")).sessions.find(s => s.id === session.id);
    const fileHashes = (directory) => Object.fromEntries(fs.readdirSync(directory, { recursive: true, withFileTypes: true })
      .filter(entry => entry.isFile()).map(entry => {
        const file = path.join(entry.parentPath || entry.path, entry.name);
        return [path.relative(directory, file), createHash("sha256").update(fs.readFileSync(file)).digest("hex")];
      }));
    const preserved = fileHashes(accountDirectory);
    await page.evaluate(() => window.sync());
    await card.getByRole("button", { name: "Hesaplar", exact: true }).click();
    await dialog.locator(`[data-account-id="${account.id}"]`).getByRole("button", { name: "Kaldır", exact: true }).click();
    await dialog.locator(`[data-account-id="${account.id}"]`).waitFor({ state: "hidden" });
    const removedProvider = (await api("app", "providers")).find(p => p.id === provider);
    assert(!removedProvider.accounts.some(a => a.id === account.id));
    assert(removedProvider.removedAccounts.some(a => a.id === account.id));
    assert.deepEqual(fileHashes(accountDirectory), preserved);
    assert.deepEqual((await api("app", "state")).sessions.find(s => s.id === session.id), retainedSession);
    assert((await api("history", "list", { id: session.id })).some(row => row.id === conversationId));
    const retainedHistory = await api("history", "messages", { id: session.id });
    assert.equal(retainedHistory.conversationId, conversationId);
    assert(retainedHistory.messages.some(message => message.text.includes("Synthetic preserved history")));
    for (const [domain, action, args] of [
      ["terminal", "start", { id: session.id }], ["app", "provider_usage", { provider, accountId: account.id }],
      ["provider_login", "start", { provider, accountId: account.id }],
      ["app", "create_session", { projectId: p.id, provider, accountId: account.id }],
    ]) await assert.rejects(api(domain, action, args), /Workflow’dan kaldırılmış/);
    await dialog.locator(".removed-provider-accounts summary").click();
    await dialog.locator(`[data-removed-account-id="${account.id}"]`).getByRole("button", { name: "Geri ekle", exact: true }).click();
    await dialog.locator(`[data-account-id="${account.id}"]`).waitFor();
    await dialog.locator("#dialog-footer button").click();
    assert.equal(await quota(account.id), managedQuota);
    const resumed = await api("terminal", "start", { id: session.id });
    pids.push(resumed.pid);
    assert.equal((await api("app", "state")).sessions.find(s => s.id === session.id).accountId, account.id);
  }
  console.log("PASS three-provider account removal and restore, live/login guards, retained files/history, and no account fallback");
  const changedAuth = path.join(data, "accounts", "COPILOT_HOME", addedAccounts.copilot, "auth-status.json");
  const originalAuth = fs.readFileSync(changedAuth);
  fs.writeFileSync(changedAuth, JSON.stringify({ login: "fixture" }));
  assert.equal((await api("app", "provider_usage", {
    provider: "copilot", accountId: addedAccounts.copilot,
  })).state, "signed-out");
  await assert.rejects(api("app", "provider_account_verify", {
    provider: "copilot", accountId: addedAccounts.copilot,
  }), /varsayılan giriş/);
  fs.writeFileSync(changedAuth, originalAuth);
  console.log("PASS independent provider accounts, logins, quotas and terminals");
  const activeBeforeLanguageChange = (await api("app", "state")).active;
  await changeLanguage("en");
  await page.getByRole("button", { name: "+ New session", exact: true }).click();
  await dialog.getByRole("heading", { name: "New session", exact: true }).waitFor();
  assert.equal(await dialog.getByLabel("Account", { exact: true }).locator("option").first().textContent(), "Default account");
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await page.locator('[data-provider="codex"] .usage-window').getByText("Weekly", { exact: true }).first().waitFor();
  await page.reload();
  await page.waitForFunction(() => document.documentElement.lang === "en" && !!window.workflow);
  await settings.click();
  await dialog.getByRole("heading", { name: "Settings", exact: true }).waitFor();
  assert.equal(await language.inputValue(), "en");
  await dialog.getByRole("button", { name: "Close", exact: true }).last().click();
  assert.deepEqual((await api("app", "state")).active, activeBeforeLanguageChange);
  await changeLanguage("tr");
  await page.getByRole("button", { name: "+ Yeni oturum", exact: true }).waitFor();
  console.log("PASS English and Turkish UI, persisted language, quota labels and live terminals");
  const rulesDir = path.join(temporary, "rules-project");
  fs.mkdirSync(rulesDir);
  const manualInstructions = "\ufeffExisting project rules\r\n";
  fs.writeFileSync(path.join(rulesDir, "AGENTS.md"), manualInstructions);
  fs.writeFileSync(path.join(rulesDir, "AGENTS.override.md"), "");
  fs.writeFileSync(path.join(rulesDir, "CLAUDE.md"), "Existing Claude project rules");
  const rulesProject = await api("app", "add_project", { directory: rulesDir });
  const getRules = (projectId) => api("app", "project_instructions_read", { projectId });
  const saveRules = (projectId, text, revision) => api("app", "project_instructions_save", { projectId, text, revision });
  await page.evaluate((id) => window.workflow.selectProject(id), rulesProject.id);
  const sessionHeader = page.locator(".session-list-heading");
  await sessionHeader.getByRole("button", { name: "Proje talimatları" }).click();
  await dialog.getByLabel("Ortak talimatlar").fill("Run the project checks.");
  await dialog.getByRole("button", { name: "Kaydet" }).click();
  await dialog.waitFor({ state: "hidden" });
  const firstRules = await getRules(rulesProject.id);
  assert.equal(firstRules.text, "Run the project checks.");
  const ruleSessions = [];
  const preLaunchMessage = "Workflow shared instructions are present at process startup.";
  const updatedMessage = "Updated Workflow instructions are present at process startup.";
  const terminalText = async (session) => {
    const attached = await api("terminal", "attach", { id: session.id });
    return Buffer.from(attached.data, "base64").toString();
  };
  for (const provider of ["codex", "claude", "copilot"]) {
    const session = await api("app", "create_session", { projectId: rulesProject.id, provider, name: provider + " shared rules", tools: false });
    const run = await api("terminal", "start", { id: session.id });
    pids.push(run.pid);
    await until(async () => (await terminalText(session)).includes(preLaunchMessage), provider + " instructions at process startup");
    if (provider === "codex")
      assert((await terminalText(session)).includes("Codex custom instructions were preserved."));
    assert.equal(fs.readFileSync(path.join(session.directory, "AGENTS.md"), "utf8"), manualInstructions);
    assert(!fs.existsSync(path.join(session.directory, ".claude")));
    assert(!fs.existsSync(path.join(session.directory, ".github")));
    assert.deepEqual(await api("files", "changes", { sessionId: session.id }), []);
    ruleSessions.push({ session, run });
  }
  assert.equal(fs.readFileSync(path.join(rulesDir, "AGENTS.md"), "utf8"), manualInstructions);
  assert.equal(fs.readFileSync(path.join(rulesDir, "CLAUDE.md"), "utf8"), "Existing Claude project rules");
  assert(!fs.existsSync(path.join(rulesDir, ".claude")));
  await saveRules(rulesProject.id, "Run the updated project checks.", firstRules.revision);
  await assert.rejects(saveRules(rulesProject.id, "Outdated editor", firstRules.revision), /Editörü yeniden aç/);
  const currentRules = await getRules(rulesProject.id);
  const [codexRules, claudeRules] = ruleSessions;
  const attachedAgain = await api("terminal", "start", { id: codexRules.session.id });
  assert.equal(attachedAgain.runId, codexRules.run.runId);
  assert(!(await terminalText(codexRules.session)).includes(updatedMessage));
  for (const { session, run } of ruleSessions)
    await api("terminal", "stop", { id: session.id, runId: run.runId });
  await until(async () => !(await api("app", "state")).active.some((run) =>
    ruleSessions.some(({ session }) => run.id === session.id)), "instructions sessions stopped");
  const resumedRules = await api("terminal", "start", { id: codexRules.session.id });
  pids.push(resumedRules.pid);
  assert.notEqual(resumedRules.runId, codexRules.run.runId);
  await until(async () => (await terminalText(codexRules.session)).includes(updatedMessage), "updated instructions at process startup");
  assert.equal(fs.readFileSync(path.join(codexRules.session.directory, "AGENTS.override.md"), "utf8"), "");
  await api("terminal", "stop", { id: codexRules.session.id, runId: resumedRules.runId });
  const resumedClaudeRules = await api("terminal", "start", { id: claudeRules.session.id });
  pids.push(resumedClaudeRules.pid);
  assert.notEqual(resumedClaudeRules.runId, claudeRules.run.runId);
  await until(async () => (await terminalText(claudeRules.session)).includes(updatedMessage), "Claude receives updated instructions");
  await api("terminal", "stop", { id: claudeRules.session.id, runId: resumedClaudeRules.runId });

  const worktreeRules = await api("app", "create_session", { projectId: gp.id, provider: "claude", name: "Rules in worktree", tools: false });
  assert.equal(worktreeRules.workspaceKind, "worktree");
  const gitRules = await getRules(gp.id);
  await saveRules(gp.id, "Verify changes before committing.", gitRules.revision);
  const directGitRules = await api("app", "create_session", { projectId: gp.id, provider: "copilot", workspaceMode: "direct", name: "Rules in Git project", tools: false });
  for (const session of [worktreeRules, directGitRules]) {
    const context = { sessionId: session.id };
    const before = await api("git", "state", context);
    assert(before.available);
    const run = await api("terminal", "start", { id: session.id });
    pids.push(run.pid);
    await until(async () => (await terminalText(session)).includes(preLaunchMessage), "Git workspace receives rules");
    assert.deepEqual((await api("git", "state", context)).files, before.files);
    assert.deepEqual(await api("files", "changes", context), []);
    await api("terminal", "stop", { id: session.id, runId: run.runId });
  }

  const directRules = await api("app", "create_session", { projectId: rulesProject.id, provider: "codex", workspaceMode: "direct", name: "Rules in existing folder", tools: false });
  const overridePath = path.join(rulesDir, "AGENTS.override.md");
  const digest = (value) => createHash("sha256").update(value).digest("hex");
  const legacyRules = (prefix) => {
    const body = "A shared instruction from a previous version.\n";
    return `${prefix}<!-- workflow:begin -->\n<!-- workflow:sha256 ${digest(body)} ${digest(prefix)} -->\n${body}<!-- workflow:end -->\n`;
  };
  fs.writeFileSync(overridePath, legacyRules("Handwritten override\n"));
  const oldCopilotRules = path.join(rulesDir, ".github", "copilot-instructions.md");
  fs.mkdirSync(path.dirname(oldCopilotRules));
  fs.writeFileSync(oldCopilotRules, legacyRules(""));
  const directRulesRun = await api("terminal", "start", { id: directRules.id });
  pids.push(directRulesRun.pid);
  await until(async () => (await terminalText(directRules)).includes(updatedMessage), "existing-folder rules at startup");
  assert.equal(fs.readFileSync(overridePath, "utf8"), "Handwritten override");
  assert(!fs.existsSync(oldCopilotRules));
  assert.equal(fs.readFileSync(path.join(rulesDir, "AGENTS.md"), "utf8"), manualInstructions);
  await api("terminal", "stop", { id: directRules.id, runId: directRulesRun.runId });
  await until(async () => !(await api("app", "state")).active.some((run) => run.id === directRules.id), "direct rules session stopped");
  await saveRules(rulesProject.id, "", currentRules.revision);
  const clearedRulesRun = await api("terminal", "start", { id: directRules.id });
  pids.push(clearedRulesRun.pid);
  await until(async () => (await terminalText(directRules)).includes("Ready >"), "session starts with cleared rules");
  assert(!(await terminalText(directRules)).includes(updatedMessage));
  assert.equal(fs.readFileSync(overridePath, "utf8"), "Handwritten override");
  await api("terminal", "stop", { id: directRules.id, runId: clearedRulesRun.runId });
  await until(async () => !(await api("app", "state")).active.some((run) =>
    run.id === claudeRules.session.id), "Claude instructions session stopped");
  const clearedClaudeRules = await api("terminal", "start", { id: claudeRules.session.id });
  pids.push(clearedClaudeRules.pid);
  assert.notEqual(clearedClaudeRules.runId, resumedClaudeRules.runId);
  await until(async () => (await terminalText(claudeRules.session)).includes("Ready >"), "Claude starts after clearing rules");
  const clearedClaudeText = await terminalText(claudeRules.session);
  assert(clearedClaudeText.includes("Claude reloads the current instructions."));
  assert(!clearedClaudeText.includes(updatedMessage));
  await api("terminal", "stop", { id: claudeRules.session.id, runId: clearedClaudeRules.runId });
  await until(async () => !(await api("app", "state")).active.some((run) =>
    [...ruleSessions.map(({ session }) => session.id), worktreeRules.id, directGitRules.id, directRules.id].includes(run.id)), "shared instructions test terminals stopped");
  console.log("PASS project session actions, external instructions for all three providers, unchanged Git state, reattach, and legacy cleanup");
  const securityProjectPath = path.join(temporary, "security-template-project");
  fs.mkdirSync(securityProjectPath);
  const securityProject = await api("app", "add_project", { directory: securityProjectPath });
  await page.evaluate((id) => window.workflow.selectProject(id), securityProject.id);
  await openTemplateMenu();
  await page.locator("#session-template-items").getByRole("button", { name: "Güvenlik Taraması", exact: true }).click();
  assert.equal(await dialog.getByLabel("Şablon", { exact: true }).inputValue(), "builtin:3");
  await dialog.getByRole("button", { name: "Oturumları oluştur" }).click();
  await dialog.waitFor({ state: "hidden" });
  const securityState = await api("app", "state");
  const securitySessions = securityState.sessions.filter((item) => item.projectId === securityProject.id);
  assert.deepEqual(securitySessions.map((item) => item.name),
    ["Code Security Review", "Dependencies & Configuration", "Findings Summary"]);
  assert(securitySessions.every((item) => item.provider === "claude" && item.initialPrompt && item.workspaceKind === "copy"));
  const [codeReview, dependencyReview, findingsSummary] = securitySessions;
  const securityIds = new Set(securitySessions.map((item) => item.id));
  assert(!securityState.active.some((run) => securityIds.has(run.id)));
  const securityLinks = securityState.links.filter((link) => securityIds.has(link.source) || securityIds.has(link.target));
  assert.deepEqual(securityLinks.map(({ source, target, read, files, work }) => ({ source, target, read, files, work })), [
    { source: findingsSummary.id, target: codeReview.id, read: true, files: false, work: false },
    { source: findingsSummary.id, target: dependencyReview.id, read: true, files: false, work: false },
  ]);
  console.log("PASS built-in security template creates three idle workspaces and conversation-only links");
  const templateProjectPath = path.join(temporary, "template-project");
  fs.mkdirSync(templateProjectPath);
  const templateProject = await api("app", "add_project", { directory: templateProjectPath });
  await page.evaluate((id) => window.workflow.selectProject(id), templateProject.id);
  await page.locator("#new-session").click();
  await dialog.getByRole("heading", { name: "Yeni oturum", exact: true }).waitFor();
  assert(!(await page.locator("#session-template-menu").isVisible()));
  await dialog.getByRole("button", { name: "Vazgeç", exact: true }).click();
  await openTemplateMenu();
  await page.locator("#session-template-items [data-template-id='builtin:1']").click();
  await dialog.getByRole("heading", { name: "İş akışı şablonları" }).waitFor();
  assert.equal(await dialog.getByLabel("Şablon", { exact: true }).inputValue(), "builtin:1");
  const implementationProvider = dialog.getByLabel("Implementation · Ayrı klasör", { exact: true });
  const reviewProvider = dialog.getByLabel("API Review · Ayrı klasör", { exact: true });
  const browserProvider = dialog.getByLabel("Browser QA · Ayrı klasör", { exact: true });
  assert.equal(await implementationProvider.inputValue(), "claude");
  assert.equal(await reviewProvider.inputValue(), "claude");
  assert.equal(await browserProvider.inputValue(), "copilot");
  await implementationProvider.selectOption("codex");
  await dialog.getByRole("button", { name: "Oturumları oluştur" }).click();
  await dialog.waitFor({ state: "hidden" });
  const afterTemplate = await api("app", "state");
  const created = afterTemplate.sessions.filter((item) => item.projectId === templateProject.id);
  assert.equal(created.length, 5);
  assert(created.every((item) => item.initialPrompt && item.workspaceKind === "copy"));
  assert.deepEqual(created.map((item) => item.provider), ["claude", "codex", "claude", "copilot", "claude"]);
  const bySession = (runs) => [...runs].sort((a, b) => a.id.localeCompare(b.id));
  assert.deepEqual(bySession(afterTemplate.active), bySession(activeBeforeLanguageChange));
  const ids = new Set(created.map((item) => item.id));
  assert.equal(afterTemplate.links.filter((link) => ids.has(link.source) && ids.has(link.target)).length, 5);
  const capture = await api("templates", "capture", { projectId: templateProject.id, name: "Reusable map" });
  const exportPath = path.join(temporary, "workflow-template.json");
  await api("templates", "export", { id: capture.id, path: exportPath });
  await assert.rejects(api("templates", "export", { id: capture.id, path: exportPath }), /yeni bir dosya adı/);
  const exported = fs.readFileSync(exportPath, "utf8");
  assert(!/accountId|conversationId|directory|projectId|selectedProviders/.test(exported));
  assert(!exported.includes(created[0].id));
  const imported = await api("templates", "import", { path: exportPath });
  assert.deepEqual(imported.template, capture.template);
  await openTemplateManager();
  const templateChoice = dialog.getByLabel("Şablon", { exact: true });
  const editTemplateButton = dialog.getByRole("button", { name: "Şablonu düzenle" });
  const deleteTemplateButton = dialog.getByRole("button", { name: "Şablonu sil" });
  assert.equal(await editTemplateButton.isVisible(), false);
  assert.equal(await deleteTemplateButton.isVisible(), false);
  await templateChoice.selectOption("builtin:0");
  assert.equal(await implementationProvider.inputValue(), "claude");
  assert.equal(await dialog.getByLabel("Review · Ayrı klasör", { exact: true }).inputValue(), "claude");
  await templateChoice.selectOption("builtin:1");
  assert.equal(await implementationProvider.inputValue(), "codex");
  await implementationProvider.selectOption("claude");
  await dialog.getByRole("button", { name: "Vazgeç", exact: true }).click();
  await dialog.waitFor({ state: "hidden" });
  await openTemplateManager();
  await templateChoice.selectOption("builtin:1");
  assert.equal(await implementationProvider.inputValue(), "codex");
  for (const action of ["update", "delete"]) {
    await assert.rejects(api("templates", action, { id: "builtin:1" }), /Hazır şablonlar/);
  }
  await templateChoice.selectOption(imported.id);
  await editTemplateButton.click();
  await dialog.getByLabel("Şablon adı", { exact: true }).fill("Updated map");
  await dialog.getByLabel("Oturum adı", { exact: true }).first().fill("Acceptance");
  await dialog.getByText("Implementation → Requirements", { exact: true }).click();
  await dialog.getByRole("checkbox", { name: "Dosya", exact: true }).check();
  await dialog.getByRole("button", { name: "Kaydet", exact: true }).click();
  await templateChoice.waitFor();
  assert.equal(await templateChoice.inputValue(), imported.id);
  const updated = (await api("templates", "list")).find((entry) => entry.id === imported.id);
  assert.equal(updated.template.name, "Updated map");
  assert.equal(updated.template.sessions[0].name, "Acceptance");
  assert.equal(updated.template.links[0].files, true);
  await assert.rejects(api("templates", "delete", { id: imported.id, revision: imported.revision }), /Şablon değişti/);
  await editTemplateButton.click();
  await dialog.getByRole("checkbox", { name: "Oturumları ve bağlantıları mevcut haritadan al" }).check();
  await dialog.getByRole("button", { name: "Kaydet", exact: true }).click();
  await templateChoice.waitFor();
  const recaptured = (await api("templates", "list")).find((entry) => entry.id === imported.id);
  assert.equal(recaptured.template.name, "Updated map");
  assert.deepEqual(recaptured.template.sessions, capture.template.sessions);
  assert.deepEqual(recaptured.template.links, capture.template.links);
  await deleteTemplateButton.click();
  await dialog.getByRole("button", { name: "Vazgeç", exact: true }).click();
  await templateChoice.waitFor();
  assert.equal(await templateChoice.inputValue(), imported.id);
  await deleteTemplateButton.click();
  await dialog.getByRole("button", { name: "Sil", exact: true }).click();
  await templateChoice.waitFor();
  const templates = await api("templates", "list");
  assert.equal(templates.filter((entry) => entry.id.startsWith("builtin:")).length, 4);
  assert(templates.some((entry) => entry.id === capture.id));
  assert(!templates.some((entry) => entry.id === imported.id));
  const afterDeletion = await api("app", "state");
  assert.deepEqual(afterDeletion.sessions, afterTemplate.sessions);
  assert.deepEqual(afterDeletion.links, afterTemplate.links);
  await dialog.getByRole("button", { name: "Vazgeç", exact: true }).click();
  await dialog.waitFor({ state: "hidden" });
  console.log("PASS custom template edit, recapture, stale revision, safe deletion and built-in protection");
  console.log("PASS template UI, five-session map, isolated creation and portable import/export");
  const out = path.join(root, "test-results");
  fs.mkdirSync(out, { recursive: true });
  await page.evaluate(() => window.workflow.openPanel("browser"));
  await page.screenshot({ path: path.join(out, "Workflow.png") });
  await api("preview", "start", { sessionId: s.id });
  await until(
    async () => {
      const run = await api("preview", "status", { sessionId: s.id });
      return run.state === "ready" && run.pageId;
    },
    "saved preview command starts again",
    15000,
  );
  await stop();
  await until(async () => {
    try {
      await fetch(previewUrl);
      return false;
    } catch {
      return true;
    }
  }, "application exit stops preview descendants");
  for (const pid of pids) {
    let alive = true;
    try {
      process.kill(pid, 0);
    } catch {
      alive = false;
    }
    assert(!alive, "provider remains after exit " + pid);
  }
  await start();
  await page.getByRole("button", { name: "+ Yeni oturum", exact: true }).waitFor();
  assert.equal((await api("app", "state")).active.length, 0);
  await openTemplateManager();
  const restoredTemplateDialog = page.locator("#dialog");
  await restoredTemplateDialog.getByLabel("Şablon", { exact: true }).selectOption("builtin:1");
  assert.equal(await restoredTemplateDialog.getByLabel("Implementation · Ayrı klasör", { exact: true }).inputValue(), "codex");
  assert.equal(await restoredTemplateDialog.getByLabel("Browser QA · Ayrı klasör", { exact: true }).inputValue(), "copilot");
  await restoredTemplateDialog.getByRole("button", { name: "Vazgeç", exact: true }).click();
  console.log("PASS template provider defaults, saved per-role selection, cancellation and restart");
  for (const { id, accounts } of await api("app", "providers")) {
    assert.equal(accounts[0].id, "default");
    assert(accounts.some((a) => a.id === addedAccounts[id] && a.verified));
  }
  assert.deepEqual(await api("browser", "trace_summary", { sessionId: s.id }), {
    steps: steps.map((step) => ({ ...step, hasImage: false })),
    passed: 1,
    failed: 1,
    total: 5,
  });
  await api("browser", "trace_clear", { sessionId: s.id });
  assert.deepEqual(await api("browser", "trace_summary", { sessionId: s.id }), {
    steps: [],
    passed: 0,
    failed: 0,
    total: 0,
  });
  console.log("PASS session summary diff, browser verification and restart persistence");
  assert.equal(await api("preview", "status", { sessionId: s.id }), null);
  assert.equal(
    (await api("app", "state")).sessions.find((item) => item.id === s.id)
      .preview.url,
    previewUrl,
  );
  await stop();
  const setupProfile = path.join(temporary, "clean-setup-profile");
  await start("codex,claude,copilot", setupProfile);
  const setupDialog = page.locator("#dialog");
  await setupDialog.getByRole("heading", { name: "Provider setup" }).waitFor();
  await setupDialog.getByRole("button", { name: "Skip", exact: true }).click();
  await page.locator("#app-settings").click();
  await setupDialog.getByRole("button", { name: "Provider setup", exact: true }).click();
  await setupDialog.getByText("Detect", { exact: true }).waitFor();
  await setupDialog.getByRole("button", { name: "Next", exact: true }).click();
  await setupDialog.getByText(/Anthropic\.ClaudeCode/).waitFor();
  await assert.rejects(api("app", "provider_install", { provider: "claude", method: "winget" }), /önce onayla/);
  await setupDialog.getByRole("button", { name: "Skip", exact: true }).click();
  await openTemplateManager();
  await setupDialog.getByText("Claude Code is not installed. Choose another provider.").first().waitFor();
  assert.equal(await setupDialog.getByRole("button", { name: "Create sessions" }).isDisabled(), true);
  await stop();
  await start("claude,copilot", setupProfile);
  const alternateProject = await api("app", "add_project", { directory: templateProjectPath });
  await openTemplateManager();
  const alternateDialog = page.locator("#dialog");
  await alternateDialog.getByLabel("Implementation · Separate folder").selectOption("codex");
  await alternateDialog.getByLabel("Review · Separate folder").selectOption("codex");
  await alternateDialog.getByRole("button", { name: "Create sessions" }).click();
  await alternateDialog.waitFor({ state: "hidden" });
  const alternateSessions = (await api("app", "state")).sessions.filter((item) => item.projectId === alternateProject.id);
  assert.deepEqual(alternateSessions.map((item) => item.provider), ["codex", "codex"]);
  await stop();
  console.log("PASS setup detection, installer confirmation guard and unavailable template providers");
  console.log("PASS application exit closes all providers; reopen starts none");
  fs.writeFileSync(
    path.join(out, "integration.json"),
    JSON.stringify(
      {
        passed: true,
        calls,
        profile: temporary,
        finishedAt: new Date().toISOString(),
      },
      null,
      2,
    ),
  );
}
test()
  .catch(async (e) => {
    await page
      ?.screenshot({ path: path.join(root, "test-results/Failure.png") })
      .catch(() => {});
    console.error(e);
    process.exitCode = 1;
  })
  .finally(async () => {
    try {
      await stop();
    } catch {
      if (child?.pid)
        cp.spawnSync("taskkill", ["/pid", String(child.pid), "/t", "/f"], {
          windowsHide: true,
        });
    }
    await new Promise((r) => (server ? server.close(r) : r()));
    console.log("Test directory:", temporary);
  });

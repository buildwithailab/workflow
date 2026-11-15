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
async function start(missingProviders = "", profile = data) {
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
  assert.equal((await widths()).left, 180);
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
  await page.locator("#projects").getByText(retainedSession.name, { exact: true }).waitFor();
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
    archived: true,
  });
  await page.evaluate(() => window.workflow.refresh());
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
  await historyButton.click();
  await until(
    async () =>
      (await page.locator("#dialog-title").textContent()) ===
      archivedName + " · Geçmiş",
    "archived conversation history",
  );
  await page.locator("#dialog-close").click();
  await page.locator("#archive").click();
  await restoreButton.click();
  await loginCard.waitFor({ state: "visible" });
  assert.equal(
    (await api("app", "state")).active.some((a) => a.id === login.id),
    false,
  );
  console.log(
    "PASS archive actions fit, open history, and restore without launching the terminal",
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
  }
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
  const projectRow = page.locator(".project-row.active");
  const projectLayout = await projectRow.evaluate((row) => {
    const rect = (name) => row.querySelector(name).getBoundingClientRect();
    const name = rect(".project-name");
    const count = rect(".project-session-count");
    const rules = rect(".project-rules");
    return { ordered: name.right < count.left && count.right < rules.left,
      centerOffset: Math.abs((count.top + count.bottom - rules.top - rules.bottom) / 2) };
  });
  assert(projectLayout.ordered && projectLayout.centerOffset <= 1);
  await projectRow.getByRole("button", { name: "Proje talimatları" }).click();
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
  console.log("PASS project row alignment, external instructions for all three providers, unchanged Git state, reattach, and legacy cleanup");
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

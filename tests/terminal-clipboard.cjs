// Focused regression checks using the bundled xterm and the actual input handler.
// WORKFLOW_CLIPBOARD_HELPER optionally points to a native clipboard test bridge.
const fs = require("fs"), path = require("path"), assert = require("assert/strict");
const cp = require("child_process"), { promisify } = require("util");
const execFile = promisify(cp.execFile);
const playwright = require(process.env.WORKFLOW_PLAYWRIGHT || "playwright-core");
const root = path.resolve(__dirname, "..");
const source = fs.readFileSync(path.join(root, "ui/app.js"), "utf8");
const handler = source.slice(source.indexOf("function terminalClipboard("), source.indexOf("function createCard("));
let clipboard = "", browser;
async function clipboardApi(action, args = {}) {
  if (!process.env.WORKFLOW_CLIPBOARD_HELPER) {
    if (action === "write") { clipboard = args.text; return true; }
    return clipboard;
  }
  return new Promise((resolve, reject) => {
    const child = cp.spawn(process.env.WORKFLOW_CLIPBOARD_HELPER, [], { windowsHide: true });
    let output = "", stderr = "";
    child.on("error", reject);
    child.stdout.on("data", b => { output += b; });
    child.stderr.on("data", b => { stderr += b; });
    child.on("exit", code => {
      try {
        if (code) throw Error(stderr || "Clipboard bridge exited: " + code);
        const result = JSON.parse(output.trim());
        if (!result.ok) throw Error(result.error);
        resolve(result.value);
      } catch (error) { reject(error); }
    });
    child.stdin.end(JSON.stringify({ action, args }) + "\n");
  });
}
async function externalCopy(text) {
  if (!process.env.WORKFLOW_CLIPBOARD_HELPER) { clipboard = text; return; }
  const encoded = Buffer.from(text, "utf8").toString("base64");
  const script = `Add-Type -AssemblyName System.Windows.Forms; [Windows.Forms.Clipboard]::SetText([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('${encoded}')))`;
  await execFile("powershell.exe", ["-NoProfile", "-STA", "-EncodedCommand", Buffer.from(script, "utf16le").toString("base64")], { windowsHide: true });
}
async function test() {
  browser = await playwright.chromium.launch({ channel: "msedge", headless: true });
  const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
  await page.exposeFunction("clipboardApi", clipboardApi);
  await page.setContent('<div id="host" style="width:800px;height:400px"></div>');
  await page.addStyleTag({ path: path.join(root, "ui/vendor/xterm.css") });
  await page.addStyleTag({ content: fs.readFileSync(path.join(root, "ui/style.css"), "utf8").replace(/^@import.*\r?\n/, "") });
  await page.addScriptTag({ path: path.join(root, "ui/vendor/xterm.js") });
  await page.addScriptTag({ content: `
    window.sent = []; window.errors = []; window.run = "first"; window.delay = 0;
    const $ = selector => document.querySelector(selector);
    const hideBrowser = () => {}, scheduleViewport = () => {};
    const tr = x => x, error = e => window.errors.push(String(e));
    function el(tag, cls, text) { const n = document.createElement(tag); n.className = cls; if(text !== undefined) n.textContent = text; return n; }
    function button(text, fn) { const b = el("button", "", text); b.onclick = e => { e.stopPropagation(); Promise.resolve(fn(e)).catch(error); }; return b; }
    const api = async (_, action, args) => {
      window.readStarted = action === "read";
      if (window.delay) await new Promise(r => setTimeout(r, window.delay));
      if (window.failClipboard) throw Error("clipboard busy");
      return window.clipboardApi(action, args);
    };
    ${handler}
    window.term = new Terminal({cols:80,rows:20});
    const host = document.querySelector("#host"); term.open(host);
    terminalClipboard(term, host, () => window.run, data => { window.sent.push(data); });
    term.focus();
  ` });
  const flush = async () => {
    await page.keyboard.press("F12"); // An input marker, queued after clipboard work.
    await page.waitForFunction(() => sent.at(-1) === "\x1b[24~");
    return page.evaluate(() => { const result = sent.splice(0); result.pop(); return result; });
  };
  for (const shortcut of ["Control+v", "Control+Shift+V", "Shift+Insert"]) {
    const text = "Dış uygulamadan: ığüşöç İĞÜŞÖÇ 🦖 " + shortcut;
    await externalCopy(text);
    await page.keyboard.press(shortcut);
    assert.deepEqual(await flush(), [text], shortcut + " pastes exactly once without Enter");
  }
  console.log("PASS external text, Unicode, Ctrl+V, Ctrl+Shift+V and Shift+Insert");
  const selection = "Terminalden kopyala";
  await page.evaluate(text => new Promise(resolve => {
    term.reset(); term.write(text, () => { term.select(0, 0, text.length); resolve(); });
  }), selection);
  for (const shortcut of ["Control+c", "Control+Shift+C", "Control+Insert"]) {
    await externalCopy("before " + shortcut);
    await page.evaluate(text => term.select(0, 0, text.length), selection);
    await page.keyboard.press(shortcut);
    // Copy is asynchronous and intentionally does not enter the terminal input queue.
    await assertEventually(async () => (await clipboardApi("read")) === selection);
    assert.deepEqual(await flush(), [], "copy sends no CLI interrupt");
  }
  await page.evaluate(() => term.clearSelection());
  for (const [key, expected] of [["Control+c", "\x03"], ["Control+a", "\x01"], ["Control+d", "\x04"], ["Control+l", "\x0c"], ["Escape", "\x1b"], ["ArrowUp", "\x1b[A"], ["Enter", "\r"]]) {
    await page.keyboard.press(key);
    assert.deepEqual(await flush(), [expected], key + " preserves CLI behavior");
  }
  await page.keyboard.press("Control+Shift+C");
  assert.deepEqual(await flush(), [], "copy without a selection does not interrupt");
  assert.deepEqual(await page.evaluate(() => errors.splice(0)), ["Error: Kopyalamak için terminalde metin seç."]);
  console.log("PASS copy shortcuts and standard CLI control keys");
  await externalCopy("first\r\nsecond\tthird");
  await page.keyboard.press("Control+v");
  const confirmation = page.getByRole("dialog", { name: "Terminale yapıştır" });
  await confirmation.getByRole("button", { name: "Vazgeç" }).click();
  assert.deepEqual(await flush(), [], "cancelling multiline input sends nothing");
  await page.locator(".xterm-helper-textarea").evaluate((target) => {
    const clipboardData = new DataTransfer();
    clipboardData.setData("text/plain", "first\r\nsecond\tthird");
    target.dispatchEvent(new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData }));
  });
  await confirmation.getByRole("button", { name: "Tek satır olarak yapıştır" }).click();
  assert.deepEqual(await flush(), ["first second third"], "browser paste also flattens control keys after confirmation");
  console.log("PASS unbracketed multiline keyboard and browser paste");
  await page.evaluate(() => new Promise(resolve => term.write("\x1b[?2004h", resolve)));
  await externalCopy("ilk satır\r\nikinci satır");
  await page.evaluate(() => { delay = 150; });
  await page.keyboard.press("Control+v");
  await page.keyboard.press("Enter");
  assert.deepEqual(await flush(), ["\x1b[200~ilk satır\rikinci satır\x1b[201~", "\r"], "paste precedes a quickly pressed Enter");
  await page.evaluate(() => { delay = 0; });
  console.log("PASS multiline bracketed paste and input ordering");
  await externalCopy("menu paste");
  await page.locator(".xterm-screen").click({ button: "right" });
  await page.getByRole("menuitem", { name: "Yapıştır Ctrl+V" }).click();
  assert.deepEqual(await flush(), ["\x1b[200~menu paste\x1b[201~"]);
  await page.evaluate(() => { delay = 150; readStarted = false; });
  await page.keyboard.press("Control+v");
  await page.waitForFunction(() => readStarted);
  await page.evaluate(() => { run = "replacement"; });
  assert.deepEqual(await flush(), [], "pending paste never reaches a replacement provider");
  await page.evaluate(() => { delay = 0; run = null; });
  await page.keyboard.press("Control+v");
  await page.keyboard.press("Control+c");
  await page.waitForTimeout(30);
  assert.deepEqual(await page.evaluate(() => sent), [], "stopped provider receives no input");
  assert.deepEqual(await page.evaluate(() => errors.splice(0)), ["Yapıştırmak için oturumun terminalini başlat."]);
  await page.evaluate(() => { run = "replacement"; failClipboard = true; });
  await page.keyboard.press("Control+v");
  assert.deepEqual(await flush(), [], "failed clipboard read does not send control codes");
  assert.deepEqual(await page.evaluate(() => errors.splice(0)), ["Error: clipboard busy"]);
  await page.evaluate(() => { failClipboard = false; });
  await page.keyboard.type("still typing");
  assert.equal((await flush()).join(""), "still typing");
  console.log("PASS right-click paste, stale/stopped runs and error recovery");
  assert.deepEqual(await page.evaluate(() => errors), []);
}
async function assertEventually(check) {
  for (let i = 0; i < 100; i++) {
    if (await check()) return;
    await new Promise(resolve => setTimeout(resolve, 30));
  }
  throw Error("Clipboard assertion timed out");
}
test().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => browser?.close());

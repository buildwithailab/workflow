const fs = require("fs"), path = require("path"), vm = require("vm");
const assert = require("node:assert/strict"), { test } = require("node:test");
const source = fs.readFileSync(path.join(__dirname, "../ui/app.js"), "utf8");
const handler = source.slice(source.indexOf("function terminalClipboard("), source.indexOf("function confirmTerminalPaste("));
const tick = () => new Promise(resolve => setImmediate(resolve));
function fixture() {
  const sent = [], errors = [], calls = [], pastes = [];
  let selection = "", run = "run-1", onData, onKey;
  const node = () => ({
    setAttribute() {}, append() {}, addEventListener() {}, focus() {},
    hidePopover() {}, showPopover() {}, style: {}, isConnected: true,
  });
  const host = node();
  const term = {
    modes: { bracketedPasteMode: true },
    options: {},
    onData(fn) { onData = fn; },
    attachCustomKeyEventHandler(fn) { onKey = fn; },
    getSelection: () => selection,
    hasSelection: () => !!selection,
    paste(text) { pastes.push(text); onData("paste:" + text); },
    scrollPages() {},
  };
  const state = { text: "clipboard text", read: null, write: null, discarded: 0 };
  const { hasPendingInput } = vm.runInNewContext(handler + "; terminalClipboard(term, host, writableRun, onInput)", {
    term, host, writableRun: () => run,
    confirmTerminalPaste: () => state.confirm?.() ?? true,
    discardScreenshot: async () => { state.discarded++; },
    onInput: data => { sent.push(data); return state.write?.(); },
    el: node, button: node, tr: value => value, error: e => errors.push(typeof e === "string" ? e : e.message),
    api: async (domain, action, args) => {
      calls.push({ domain, action, args });
      if (action === "read") return state.read ? state.read() : state.text;
      state.text = args.text;
    },
  });
  function key(key, mods = {}) {
    let prevented = false;
    const handled = !onKey({ type: "keydown", key, ...mods, preventDefault() { prevented = true; } });
    return { handled, prevented };
  }
  return { sent, errors, calls, pastes, host, state, key, hasPendingInput,
    setBracketed: value => { term.modes.bracketedPasteMode = value; },
    data: data => onData(data), select: text => { selection = text; },
    run: value => { run = value; } };
}
test("copy uses Windows clipboard without emitting a CLI interrupt", async () => {
  const f = fixture(); f.select("Türkçe 🦖");
  for (const [key, mods] of [["c", {ctrlKey:true}], ["C", {ctrlKey:true,shiftKey:true}], ["Insert", {ctrlKey:true}]]) {
    f.state.text = "old";
    assert.deepEqual(f.key(key, mods), { handled:true, prevented:true });
    await tick();
    assert.equal(f.state.text, "Türkçe 🦖");
  }
  assert.deepEqual(f.sent, []);
});
test("xterm focus reports do not block actions on an empty draft", async () => {
  const f = fixture();
  f.data("\x1b[I");
  f.data("\x1b[O");
  assert.equal(f.hasPendingInput(), false);
  f.data("draft");
  assert.equal(f.hasPendingInput(), true);
  await tick();
  assert.deepEqual(f.sent, ["\x1b[I", "\x1b[O", "draft"]);
  assert.equal(f.hasPendingInput(), false);
});
test("unselected Ctrl+C and ordinary CLI shortcuts remain with xterm", async () => {
  const f = fixture();
  for (const key of ["c", "a", "d", "l", "z", "r"]) {
    assert.deepEqual(f.key(key, {ctrlKey:true}), { handled:false, prevented:false });
  }
  for (const key of ["Enter", "Escape", "ArrowUp", "Backspace", "Tab"]) {
    assert.equal(f.key(key).handled, false);
  }
  assert.equal(f.key("v", {ctrlKey:true,altKey:true}).handled, false);
  assert.equal(f.key("C", {ctrlKey:true,shiftKey:true}).handled, true);
  assert.deepEqual(f.calls, []);
  await tick();
  assert.deepEqual(f.errors, ["Kopyalamak için terminalde metin seç."]);
});
test("paste shortcuts route text through xterm once, without appending Enter", async () => {
  const f = fixture(); f.state.text = "ilk\r\nikinci 🦖";
  for (const [key, mods] of [["v", {ctrlKey:true}], ["V", {ctrlKey:true,shiftKey:true}], ["Insert", {shiftKey:true}]]) {
    assert.deepEqual(f.key(key, mods), { handled:true, prevented:true });
    await tick();
  }
  assert.deepEqual(f.pastes, Array(3).fill(f.state.text));
  assert.deepEqual(f.sent, Array(3).fill("paste:" + f.state.text));
});
test("asynchronous paste completes before subsequent typing and Enter", async () => {
  const f = fixture(); let resolveRead, resolveWrite;
  f.state.read = () => new Promise(resolve => { resolveRead = resolve; });
  f.state.write = () => new Promise(resolve => { resolveWrite = resolve; });
  f.key("v", {ctrlKey:true});
  f.data("suffix"); f.data("\r");
  await tick(); assert.deepEqual(f.sent, []);
  resolveRead("draft"); await tick();
  assert.deepEqual(f.sent, ["paste:draft"]);
  f.state.write = null; resolveWrite(); await tick();
  assert.deepEqual(f.sent, ["paste:draft", "suffix", "\r"]);
});
test("pending paste and queued input are discarded when provider run changes", async () => {
  const f = fixture(); let resolveRead;
  f.state.read = () => new Promise(resolve => { resolveRead = resolve; });
  f.key("v", {ctrlKey:true}); f.data("\r");
  await tick(); f.run("run-2"); resolveRead("stale draft"); await tick();
  assert.deepEqual(f.pastes, []); assert.deepEqual(f.sent, []);
  assert.equal(f.state.discarded,1); assert.deepEqual(f.errors,[]);
  f.data("new input"); await tick(); assert.deepEqual(f.sent, ["new input"]);
});
test("stopped and detached terminals never receive paste", async () => {
  const f = fixture(); f.run(null);
  assert.equal(f.key("v", {ctrlKey:true}).handled, true);
  f.data("\r"); await tick();
  assert.deepEqual(f.calls, []); assert.deepEqual(f.sent, []);
  assert.deepEqual(f.errors, ["Yapıştırmak için oturumun terminalini başlat."]);
  f.run("run-2"); f.host.isConnected = false;
  f.key("v", {ctrlKey:true}); await tick(); assert.deepEqual(f.pastes, []);
});
test("an empty Windows clipboard reports the problem and keeps normal terminal input working", async () => {
  const f = fixture(); f.state.text = "";
  f.key("v", {ctrlKey:true}); f.data("normal typing"); await tick();
  assert.deepEqual(f.errors, ["Windows panosundan yapıştırılabilir metin alınamadı."]);
  assert.deepEqual(f.sent, ["normal typing"]);
  assert.deepEqual(f.pastes, []);
});
test("clipboard errors are visible and do not block further terminal input", async () => {
  const f = fixture(); f.state.read = () => Promise.reject(Error("busy clipboard"));
  f.key("v", {ctrlKey:true}); f.data("normal typing"); await tick();
  assert.deepEqual(f.errors, ["busy clipboard"]);
  assert.deepEqual(f.sent, ["normal typing"]); assert.deepEqual(f.pastes, []);
});
test("confirmation flattens unsafe newlines and tabs and drops queued Enter", async () => {
  const f = fixture(); f.setBracketed(false); f.state.text = "first\r\n\tsecond\n";
  let decide;
  f.state.confirm = () => new Promise(resolve => { decide = resolve; });
  f.key("v", {ctrlKey:true}); f.data("\r");
  await tick(); assert.deepEqual(f.sent, []);
  decide(true); await tick();
  assert.deepEqual(f.sent, ["paste:first  second "]);
  f.data("\r"); await tick();
  assert.deepEqual(f.sent, ["paste:first  second ", "\r"]);
});
test("cancelled or stale confirmation and clipboard control characters send no input", async () => {
  const f = fixture(); f.setBracketed(false); f.state.text = "first\nsecond";
  f.state.confirm = () => false;
  f.key("v", {ctrlKey:true}); f.data("\r"); await tick();
  let decide;
  f.state.confirm = () => new Promise(resolve => { decide = resolve; });
  f.key("v", {ctrlKey:true}); await tick();
  f.run("run-2"); decide(true); await tick();
  f.setBracketed(true); f.state.text = ["a", String.fromCharCode(27), "b"].join("");
  f.key("v", {ctrlKey:true}); f.data("\r"); await tick();
  assert.deepEqual(f.sent, []);
  assert.equal(f.errors.length, 1);
});

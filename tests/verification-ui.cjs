const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/verification.js"), "utf8");
const settle = () => new Promise(resolve => setImmediate(resolve));

function node() {
  return {
    children: [], dataset: {}, classList: { toggle() {} },
    append(...children) { this.children.push(...children); },
    replaceChildren(...children) { this.children = children; },
    setAttribute(name, value) { this[name] = value; },
    insertBefore(child) { child.parentElement = this; },
  };
}

function fixture() {
  let selected = "first";
  let nextTimer = 0;
  const nodes = new Map();
  const timers = new Map();
  const calls = [];
  const states = new Map(["first", "second"].map(id => [id, {
    phase: "ready", revision: 2, items: [{ id: "item", title: id, check: "Check " + id }],
  }]));
  const read = new Map();
  const writes = new Map();
  function element(selector) {
    if (!nodes.has(selector)) nodes.set(selector, node());
    return nodes.get(selector);
  }
  const context = vm.createContext({
    document: { createElement: node }, $: element, tr: value => value,
    error: message => assert.fail(String(message)),
    selectedChatSession: () => ({ id: selected }), active: () => true, shownChatId: null,
    clearTimeout: id => timers.delete(id),
    setTimeout: fn => { timers.set(++nextTimer, fn); return nextTimer; },
    api: async (domain, action, args) => {
      calls.push({ domain, action, args });
      assert.equal(domain, "verification");
      if (action === "edit") {
        if(writes.has(args.id)) await writes.get(args.id);
        states.set(args.id, { phase: "ready", revision: args.revision + 1, items: args.items });
      } else {
        assert.equal(action, "state");
        if (read.has(args.id)) await read.get(args.id);
      }
      return states.get(args.id);
    },
  });
  vm.runInContext(source + "\ninitVerificationControls()", context);
  const render = () => context.renderVerificationView();
  render();
  return {
    calls, timers, states, read, writes, element,
    cached() { return vm.runInContext("[...verificationViews.values()].filter(view=>view.state).length",context); },
    size() { return vm.runInContext("verificationViews.size",context); },
    reset() { context.resetVerificationViews(); },
    select(id, chat = false) { selected = id; context.shownChatId = chat ? id : null; render(); },
    toggle() { element("#session-verification-view").onclick(); },
    action(label) {
      const button = element("#verification-actions").children.find(child => child.textContent === label);
      assert.ok(button && !button.disabled, label);
      button.onclick();
    },
    title() { return element("#verification-content").children[0].children[0]; },
  };
}

test("opening, switching terminal/chat, and closing only read the selected plan", async () => {
  const f = fixture();
  const panel = f.element("#verification-panel");
  assert.equal(panel.hidden, true);
  assert.equal(f.calls.length, 0);
  f.toggle();
  await settle();
  assert.equal(panel.hidden, false);
  assert.equal(panel.parentElement, f.element(".main"));
  f.select("first", true);
  assert.equal(panel.parentElement, f.element("#chat-view"));
  f.select("second", true);
  assert.equal(panel.hidden, true);
  f.select("first");
  await settle();
  assert.equal(panel.hidden, false);
  f.element("#verification-close").onclick();
  assert.equal(panel.hidden, true);
  assert.equal(f.timers.size, 0);
  assert.deepEqual(f.calls.map(call => [call.action, call.args.id]), [["state", "first"], ["state", "first"]]);
});

test("two drafts at the same revision retain their own fields, listeners and save target", async () => {
  const f = fixture();
  for (const id of ["first", "second"]) {
    f.select(id);
    f.toggle();
    await settle();
    f.action("Listeyi düzenle");
    const title = f.title();
    assert.equal(title.value, id);
    title.value = id + " draft";
    title.oninput();
  }
  f.select("first", true);
  await settle();
  const title = f.title();
  assert.equal(title.value, "first draft");
  title.value = "first revised";
  title.oninput();
  f.action("Listeyi kaydet");
  await settle();
  const edits = f.calls.filter(call => call.action === "edit");
  assert.equal(edits.length, 1);
  assert.equal(edits[0].args.id, "first");
  assert.equal(edits[0].args.revision, 2);
  assert.equal(edits[0].args.items[0].title, "first revised");
  f.select("second");
  await settle();
  assert.equal(f.title().value, "second draft");
});

test("a delayed response after switching sessions preserves the selected session's poll", async () => {
  const f = fixture();
  let finish;
  f.read.set("first", new Promise(resolve => { finish = resolve; }));
  f.toggle();
  f.select("second");
  f.toggle();
  await settle();
  assert.equal(f.timers.size, 1);
  const [timer, poll] = f.timers.entries().next().value;
  finish();
  await settle();
  assert.equal(f.timers.get(timer), poll);
  f.timers.delete(timer);
  await poll();
  assert.equal(f.calls.at(-1).args.id, "second");
  assert.equal(f.calls.at(-1).action, "state");
  assert.equal(f.timers.size, 1);
});

test("hundreds of opened reports keep only the visible report and closing releases its DOM", async () => {
  const f=fixture();
  for(let index=0;index<600;index++) {
    const id="synthetic-"+index;
    f.states.set(id,{phase:"reported",revision:1,items:[],results:[{evidence:"synthetic report ".repeat(1000)}]});
    f.select(id); f.toggle(); await settle();
    assert.equal(f.cached(),1);
  }
  f.toggle();
  assert.equal(f.cached(),0);
  assert.equal(f.element("#verification-content").children.length,0);
  assert.equal(f.element("#verification-actions").children.length,0);
});

test("reset discards pending reads and their late results without recreating a view or poll", async () => {
  const f=fixture();
  let finish;
  f.read.set("first",new Promise(resolve=>{finish=resolve;}));
  f.toggle();
  f.reset();
  finish(); await settle();
  assert.equal(f.size(),0);
  assert.equal(f.cached(),0);
  assert.equal(f.timers.size,0);
  assert.equal(f.element("#verification-panel").hidden,true);
  assert.equal(f.element("#verification-content").children.length,0);
});

test("reset discards a late completed edit without refetching old records", async () => {
  const f=fixture();
  f.toggle(); await settle();
  f.action("Listeyi düzenle");
  let finish;
  f.writes.set("first",new Promise(resolve=>{finish=resolve;}));
  f.action("Listeyi kaydet");
  const calls=f.calls.length;
  f.reset(); finish(); await settle();
  assert.equal(f.size(),0);
  assert.equal(f.calls.length,calls);
  assert.equal(f.timers.size,0);
});

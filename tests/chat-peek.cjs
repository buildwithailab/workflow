const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/chat.js"), "utf8");
const settle = () => new Promise(resolve => setImmediate(resolve));

function fixture(provider = "codex") {
  const nodes = new Map();
  const requests = [];
  const selected = { id: "session", conversationId: "first", provider };
  const reply = { prompt: null };
  function element(selector) {
    if (!nodes.has(selector)) nodes.set(selector, {
      dataset: {}, classList: { toggle() {} }, textContent: "",
      setAttribute(key, value) { this[key] = value; }, addEventListener() {},
      replaceChildren() {}, querySelectorAll() { return []; },
    });
    return nodes.get(selector);
  }
  const context = vm.createContext({
    $: element, selected, session: () => selected, tr: text => text, uiLanguage: "en",
    views: new Map(), starting: new Set(), active: () => true,
    db: { active: [{ id: selected.id, runId: "run" }] },
    error: value => assert.fail(String(value)), setTimeout: () => 1,
    api: async (domain, action, args) => {
      assert.equal(domain, "chat");
      assert.equal(action, "interaction");
      requests.push(args.terminalOutput);
      return { conversationId: selected.conversationId, terminalRunId: "run",
        terminalPrompt: reply.prompt, terminalOutput: args.terminalOutput ? "Example output" : null };
    },
  });
  vm.runInContext(source, context);
  // Keep the real control handlers, binding, polling and rendering code.
  Object.assign(context, { initAttachmentDrop() {}, paintChatModelControls() {}, paintChatApproval() {} });
  vm.runInContext("shownChatId = selected.id; initChatControls();", context);
  const rebind = () => context.syncChatBinding(selected, context.chatView(selected.id));
  rebind();
  return { requests, selected, reply, element, rebind,
    read: () => context.readChatInteraction(),
    toggle: () => element("#chat-show-terminal-output").onclick(),
  };
}

test("live terminal output starts collapsed and is fetched only while expanded", async () => {
  const f = fixture();
  const toggle = f.element("#chat-show-terminal-output");
  const output = f.element("#chat-terminal-output");
  await f.read();
  assert.equal(f.element("#chat-terminal-peek").hidden, true);
  assert.equal(toggle.hidden, false);
  assert.equal(toggle.textContent, "");
  assert.equal(toggle["aria-label"], "Canlı Terminal çıktısını göster");
  assert.equal(toggle["aria-expanded"], "false");
  assert.equal(output.hidden, true);
  assert.deepEqual(f.requests, [false]);
  f.toggle();
  await settle();
  assert.equal(toggle["aria-expanded"], "true");
  assert.equal(output.hidden, false);
  assert.equal(output.textContent, "Example output");
  assert.equal(f.requests.at(-1), true);
  f.toggle();
  await f.read();
  assert.equal(toggle["aria-expanded"], "false");
  assert.equal(output.hidden, true);
  assert.equal(output.textContent, "");
  assert.equal(f.requests.at(-1), false);
});

test("approval cards remain visible while the output is collapsed for every provider", async () => {
  for (const provider of ["codex", "claude", "copilot"]) {
    const f = fixture(provider);
    f.reply.prompt = { kind: "approval", text: "Example permission request" };
    await f.read();
    assert.deepEqual(f.requests, [false], provider);
    assert.equal(f.element("#chat-terminal-prompt").hidden, false, provider);
    assert.equal(f.element("#chat-terminal-prompt-text").textContent, f.reply.prompt.text);
    assert.equal(f.element("#chat-terminal-output").hidden, true);
  }
});

test("rebinding to another conversation closes the output and clears its old text", async () => {
  const f = fixture();
  await f.read();
  f.toggle();
  await settle();
  assert.equal(f.element("#chat-terminal-output").textContent, "Example output");
  f.selected.conversationId = "second";
  assert.equal(f.rebind(), true);
  await f.read();
  assert.equal(f.requests.at(-1), false);
  assert.equal(f.element("#chat-show-terminal-output")["aria-expanded"], "false");
  assert.equal(f.element("#chat-terminal-output").hidden, true);
  assert.equal(f.element("#chat-terminal-output").textContent, "");
});

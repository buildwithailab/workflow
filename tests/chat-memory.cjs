const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/chat.js"), "utf8");
const providers = ["codex", "claude", "copilot"];

function fixture(provider) {
  const selected = { id: "session", conversationId: "conversation", provider };
  const requests = [];
  const nodes = new Map();
  function element(selector) {
    if (!nodes.has(selector)) nodes.set(selector, {
      classList: { toggle() {} }, setAttribute() {}, hidePopover() {}, replaceChildren() {},
    });
    return nodes.get(selector);
  }
  const context = vm.createContext({
    $: element, focusId: selected.id, session: id => id === selected.id ? selected : null,
    active: () => true, setTimeout: () => 1, clearTimeout() {},
    renderVerificationView() {}, scheduleNotificationContext() {},
    api(domain, action, args) {
      assert.equal(domain, "chat");
      return new Promise(resolve => requests.push({ action, args, resolve }));
    },
  });
  vm.runInContext(source, context);
  // Only replace visual output. Binding, view changes and all asynchronous
  // operations run the same code as they do in the WebView.
  Object.assign(context, {
    paintChatImages() {}, paintChatTurns() {}, paintChatComposer() {}, paintChatEmpty() {},
  });
  const view = context.chatView(selected.id);
  function open(value) {
    view.open = value;
    context.renderChatView();
  }
  open(true);
  return { context, view, selected, requests, open };
}

test("the first pending transcript and interaction cannot populate a hidden or reopened chat", async () => {
  for (const provider of providers) {
    for (const reopen of [false, true]) {
      const { context, view, selected, requests, open } = fixture(provider);
      // No previously loaded transcript exists to trigger eviction on hide.
      const history = context.readChat();
      const interaction = context.readChatInteraction();
      assert.deepEqual(requests.map(r => r.action), ["read", "interaction"]);
      open(false);
      if (reopen) open(true);
      requests[0].resolve({ conversationId: selected.conversationId, found: true,
        revision: "stale", turns: [{ position: 1, prompt: "older prompt" }] });
      requests[1].resolve({ conversationId: selected.conversationId,
        terminalOutput: "stale terminal output", terminalRunId: "stale run" });
      await Promise.all([history, interaction]);
      assert.equal(view.turns.length, 0, provider);
      assert.equal(view.revision, null, provider);
      assert.equal(view.terminalOutput, null, provider);
    }
  }
});

test("hiding a chat during a successful send releases its transcript and clears the delivered draft once", async () => {
  for (const provider of providers) {
    const { context, view, selected, requests, open } = fixture(provider);
    view.draft = "my pending message";
    view.images = [{ reference: "synthetic image" }];
    view.turns = [{ position: 10, prompt: "old conversation text", state: "working" }];
    view.revision = "loaded";
    const binding = view.bindingRevision;
    const sending = context.sendChat(selected, view, view.draft, { draft: view.draft, images: view.images });
    open(false);
    assert.equal(view.turns.length, 0, provider);
    assert.equal(view.bindingRevision, binding, provider);
    assert.deepEqual(requests.map(r => r.action), ["send"]);
    requests[0].resolve(true);
    assert.equal(await sending, true, provider);
    assert.equal(view.draft, "", provider);
    assert.equal(view.images.length, 0, provider);
    assert.equal(view.pending.text, "my pending message", provider);
    open(true);
    assert.deepEqual(requests.map(r => r.action), ["send"]);
  }
});

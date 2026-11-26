const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../ui/chat.js"), "utf8");
const settle = () => new Promise(resolve => setImmediate(resolve));

function node(tag = "div", classes = "", text = "") {
  const names = new Set(classes.split(" "));
  return {
    tagName: tag.toUpperCase(), children: [], parent: null, dataset: {}, textContent: text,
    classList: { add: name => names.add(name), contains: name => names.has(name), toggle() {} },
    get isConnected() { return !!this.root || !!this.parent?.isConnected; },
    setAttribute(key, value) { this[key] = value; },
    append(...children) {
      for (const child of children) {
        this.children.push(child);
        if (typeof child === "object") child.parent = this;
      }
    },
    remove() {
      if (this.parent) this.parent.children = this.parent.children.filter(child => child !== this);
      this.parent = null;
    },
    replaceChildren(...children) {
      for (const child of this.children) if (typeof child === "object") child.parent = null;
      this.children = [];
      this.append(...children);
    },
    querySelectorAll(selector) {
      const matches = child => selector.split(", ").some(s =>
        s[0] === "." ? child.classList.contains(s.slice(1))
          : s.includes(":checked") ? child.tagName === "INPUT" && child.checked
          : child.tagName === s.toUpperCase());
      return this.children.filter(child => typeof child === "object").flatMap(child =>
        [...(matches(child) ? [child] : []), ...child.querySelectorAll(selector)]);
    },
    querySelector(selector) { return this.querySelectorAll(selector)[0]; },
  };
}

function fixture() {
  const roots = new Map();
  const requests = [];
  const errors = [];
  function element(selector) {
    if (!roots.has(selector)) roots.set(selector, Object.assign(node(), { root: true }));
    return roots.get(selector);
  }
  const selected = { id: "session", provider: "codex", conversationId: "first" };
  const run = { id: selected.id, runId: "first-run" };
  const context = vm.createContext({
    $: element, el: node, selected, focusId: selected.id, session: () => selected,
    uiLanguage: "en", views: new Map(), starting: new Set(), active: () => true,
    db: { active: [run] }, tr: text => text, icons: { codex: "" }, names: { codex: "Codex" },
    error: err => errors.push(err.message),
    api: (domain, action, args) => new Promise((resolve, reject) =>
      requests.push({ domain, action, args, resolve, reject })),
  });
  vm.runInContext(source, context);
  const poll = context.readChatInteraction;
  Object.assign(context, {
    readChat: async () => {}, readChatInteraction: async () => {},
    scheduleChatRead() {}, scheduleChatInteraction() {},
    paintChatModelControls() {}, paintChatApproval() {},
  });
  vm.runInContext("shownChatId = selected.id;", context);
  const chat = context.chatView(selected.id);
  const paint = () => context.paintChatComposer(selected, chat);
  function bind() { context.syncChatBinding(selected, chat); chat.terminalRunId = run.runId; }
  bind();
  function nativeQuestion() {
    chat.terminalPrompt = { kind: "question", text: "Which option?", menu: {
      key: "same-menu", question: "Which option?", selected: 1, inputRevision: 2,
      choices: ["First", "Second", "Other"], customChoice: 3,
    } };
    paint();
  }
  function submitCustom(card, value) {
    const input = card.querySelector(".chat-custom-input");
    input.value = value;
    input.oninput();
    const button = card.querySelector(".chat-custom-send");
    assert.equal(button.disabled, false);
    button.onclick();
    return requests.at(-1);
  }
  return { selected, run, chat, errors, requests, poll, element, bind, nativeQuestion, submitCustom, paint,
    nativeCard: () => element("#chat-terminal-choices").querySelector(".chat-question-card"),
    plainCard: () => element("#chat-turns").querySelector(".chat-reply-question"),
    renderTurns: () => { context.paintChatTurns(chat); paint(); },
  };
}

test("native custom reply stays editable on failure and closes only after success", async () => {
  const f = fixture();
  f.nativeQuestion();
  const original = f.nativeCard();
  const failed = f.submitCustom(original, "Try the sample option.");
  assert.equal(failed.action, "answer_prompt");
  assert.equal(failed.args.text, "Try the sample option.");
  assert.equal(original.isConnected, true);
  failed.reject(Error("Provider did not accept the answer"));
  await settle();
  assert.equal(f.errors.length, 1);
  assert.equal(f.nativeCard().querySelector(".chat-custom-input").value, failed.args.text);
  const retry = f.submitCustom(f.nativeCard(), failed.args.text);
  retry.resolve(true);
  await settle();
  assert.equal(f.element("#chat-terminal-prompt").hidden, true);
  assert.equal(original.isConnected, false);
  assert.equal(f.element("#chat-send").disabled, true);
});

test("success from an old binding cannot dismiss an identical menu on a replacement run", async () => {
  const f = fixture();
  f.nativeQuestion();
  const old = f.submitCustom(f.nativeCard(), "Answer from the first run");
  f.selected.conversationId = "second";
  f.run.runId = "replacement";
  f.bind();
  f.nativeQuestion();
  old.resolve(true);
  await settle();
  assert.equal(f.element("#chat-terminal-prompt").hidden, false);
  assert.ok(f.nativeCard()?.isConnected);
  assert.equal(f.chat.terminalPrompt.menu.key, "same-menu");
});

test("a poll that began before an accepted reply cannot restore its old native question", async () => {
  const f = fixture();
  f.nativeQuestion();
  const oldPrompt = structuredClone(f.chat.terminalPrompt);
  const pendingPoll = f.poll();
  const read = f.requests.at(-1);
  assert.equal(read.action, "interaction");
  const send = f.submitCustom(f.nativeCard(), "Use the first option.");
  send.resolve(true);
  await settle();
  read.resolve({ conversationId: f.selected.conversationId, terminalRunId: f.run.runId,
    terminalPrompt: oldPrompt, ticket: null });
  await pendingPoll;
  assert.equal(f.element("#chat-terminal-prompt").hidden, true);
  assert.equal(f.nativeCard(), undefined);
});

test("an older failed poll cannot erase a native question after a rejected send", async () => {
  const f = fixture();
  f.nativeQuestion();
  const oldPoll = f.poll();
  const read = f.requests.at(-1);
  assert.equal(read.action, "interaction");
  const send = f.submitCustom(f.nativeCard(), "Keep this answer for retry.");
  send.reject(Error("Provider could not submit the answer"));
  await settle();
  read.reject(Error("Previous poll timed out"));
  await oldPoll;
  assert.equal(f.element("#chat-terminal-prompt").hidden, false);
  assert.equal(f.nativeCard().querySelector(".chat-custom-input").value, send.args.text);
});

test("an accepted plain-text question stays closed on log redraw and a later turn may ask it again", async () => {
  const f = fixture();
  f.chat.ticket = { runId: f.run.runId, inputRevision: 1 };
  const turn = { prompt: "Compare the options.", answer: "Which option?\n1. First\n2. Second",
    position: 1, state: "completed" };
  f.chat.turns = [turn];
  f.renderTurns();
  const sent = f.submitCustom(f.plainCard(), "Use a different option.");
  assert.equal(sent.action, "send");
  sent.resolve(true);
  await settle();
  assert.deepEqual(f.errors, []);
  f.chat.pending = null;
  f.renderTurns();
  assert.equal(f.plainCard(), undefined);
  f.chat.turns.push({ ...turn, position: 2 });
  f.renderTurns();
  assert.ok(f.plainCard()?.isConnected);
});

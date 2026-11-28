"use strict";

const assert = require("node:assert/strict");
const { randomUUID } = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

const labels = {
  codex: ["Yes, proceed", "Yes, and don't ask again for commands that start with `echo`", "No, and tell Codex what to do differently"],
  claude: ["Yes", "Yes, and don't ask again for: echo", "No"],
  copilot: ["Yes", "Yes, and approve `echo` for this session", "No, and tell Copilot what to do differently (Esc)"],
};

module.exports = async function testChatApprovals({ start, stop, api, until, sleep, page, temporary, data, inlineDiff = false,
  rejectPrompt = (pending, pattern) => assert.rejects(pending, pattern) }) {
  await start();
  try {
    const window = page();
    const skip = window.locator("#dialog").getByRole("button", { name: "Skip", exact: true });
    if (await skip.isVisible()) await skip.click();
    for (const provider of Object.keys(labels)) {
      const folder = path.join(temporary, provider + "-approval-project");
      fs.mkdirSync(folder);
      for (const marker of ["chat-terminal-fixture", "approval-permission-fixture"]) fs.writeFileSync(path.join(folder, marker), "");
      const project = await api("app", "add_project", { directory: folder });
      const session = await api("app", "create_session", { projectId: project.id, provider, workspaceMode: "isolated", tools: false });
      const { id } = session;
      // Bind an actual generated native-history record so losing a conversation
      // cannot pass a continuity assertion just because both sides are null.
      const conversationId = randomUUID();
      const timestamp = new Date().toISOString();
      const cwd = session.directory;
      const [location, header] = {
        codex: [["test-codex-home", "sessions", "rollout-" + conversationId + ".jsonl"], { type: "session_meta", payload: { id: conversationId, cwd, timestamp } }],
        claude: [["test-claude-home", "projects", "synthetic", conversationId + ".jsonl"], { type: "user", sessionId: conversationId, cwd, timestamp, message: { role: "user", content: "Use the sample." } }],
        copilot: [["test-copilot-home", "session-state", conversationId, "events.jsonl"], { type: "session.start", timestamp, data: { sessionId: conversationId, context: { cwd } } }],
      }[provider];
      const history = path.join(data, ...location);
      fs.mkdirSync(path.dirname(history), { recursive: true });
      fs.writeFileSync(history, JSON.stringify(header) + "\n");
      await api("history", "bind", { id, conversationId });

      await window.evaluate(async id => {
        await window.workflow.refresh();
        window.workflow.selectSession(id, true);
      }, id);
      await window.locator("#session-chat-view").click();
      await window.locator("#chat-start").click();
      await until(async () => (await api("chat", "interaction", { id })).ticket, provider + " ready");
      const { runId, pid } = await api("terminal", "attach", { id });
      const input = window.locator("#chat-input");
      const send = window.locator("#chat-send");
      const card = window.locator("#chat-terminal-prompt");
      await input.fill("Inspect the sample commands.");
      await send.click();
      const artifact = name => path.join(data, "test-input", provider + "-approval-" + name);
      const signal = name => fs.writeFileSync(artifact(name), "");
      const wire = () => fs.readFileSync(artifact("wire.bin"), "latin1");
      const decision = round => fs.readFileSync(artifact("decision-" + round), "utf8");
      const prompt = async () => (await api("chat", "interaction", { id })).terminalPrompt;
      const answer = (menu, choice, extra = {}) => api("chat", "answer_prompt", { id, runId, ...menu, choice, ...extra });
      const scopedButton = choice => card.getByRole("button", { name: choice + " · " + labels[provider][choice - 1], exact: true });
      async function visibleMenu(target) {
        const menu = await until(async () => {
          const current = await prompt();
          return current?.kind === "approval" && current.menu?.question.includes(target) && current.menu;
        }, provider + " actionable approval with full target");
        await card.locator("pre").filter({ hasText: target }).waitFor();
        if (inlineDiff) {
          // Prove that this is the diff fixture, including after isolated copying.
          // A normal command-approval screen must not satisfy the diff scenario.
          for (const text of ["- previous sample", "+ " + target,
            provider === "codex" ? "$ echo fixed-sample" : "sample.rs"]) {
            assert(menu.question.includes(text), "approval identity includes the unchanged target and actual diff");
            await card.locator("pre").filter({ hasText: text }).waitFor();
          }
        }
        await card.getByText("Approval required", { exact: true }).first().waitFor();
        assert(await window.locator("#chat-terminal-output").isHidden(), "decision works with terminal preview collapsed");
        assert.deepEqual(menu.choices, labels[provider], "native decision scopes preserved");
        assert.equal(menu.descriptions[1],"Only this tool and the displayed scope.");
        await card.locator("#chat-approval-description-1").getByText("Only this tool and the displayed scope.",{exact:true}).waitFor();
        for (const choice of [1, 2, 3]) await scopedButton(choice).waitFor();
        return menu;
      }
      const draft = "Keep this follow-up in the composer.";
      for (let round = 1; round <= 4; round++) {
        if (round > 1) signal("open-" + round);
        const menu = await visibleMenu("echo sample-" + round);
        const offset = wire().length;
        const movement = () => wire().slice(offset);
        if (round === 1) {
          await input.fill(draft);
          assert(await send.isDisabled());
          await rejectPrompt(answer(menu, 1, { runId: randomUUID() }), /Oturum çalıştırması değişti/);
          await rejectPrompt(answer(menu, 1, { inputRevision: menu.inputRevision + 1 }), /Soru değişti/);
          await rejectPrompt(answer(menu, 1, { key: randomUUID() }), /Soru değişti/);
          await scopedButton(1).focus();
          await scopedButton(1).press("Tab");
          assert.equal(movement(), "", "focus navigation and stale actions write no terminal bytes");
          assert.equal((await prompt()).menu.selected, 2);
        }
        if (round === 4) {
          // Changing only the target during a selection redraw invalidates the
          // original menu even though its provider, labels and ordinal still match.
          await rejectPrompt(answer(menu, 3), /Soru değişti/);
          const updated = await visibleMenu("echo changed-target");
          assert(menu.key !== updated.key, "target is part of approval identity");
          assert.equal(updated.selected, 2);
          await rejectPrompt(answer(updated, 3, { key: menu.key }), /Soru değişti/);
          assert.equal(movement(), "\x1b[B", "changed target receives no Enter");
          await scopedButton(3).click();
        } else if (round === 1) {
          await scopedButton(1).evaluate(button => { button.click(); button.click(); });
        } else {
          await scopedButton(round).click();
        }
        if (round === 3) {
          await until(() => movement().length > 0, "first native arrow reaches fixture");
          await sleep(160);
          assert.equal(movement(), "\x1b[B", "waits for selected row to redraw before another arrow or Enter");
          assert(!fs.existsSync(artifact("decision-3")), "navigation alone is not consent");
          signal("release-redraw");
        }
        await until(() => decision(round) === String(Math.min(round, 3)), provider + " expected native decision");
        await card.waitFor({ state: "hidden" });
        await rejectPrompt(answer(menu, Math.min(round, 3)), /Soru değişti/);
        assert.equal(movement(), ["\x1b[A\r", "\x1b[B\r", "\x1b[B\x1b[B\r", "\x1b[B\x1b[B\r"][round - 1],
          "one row at a time and exactly one Enter, including after duplicate actions");
        const attached = await api("terminal", "attach", { id });
        const live = await api("chat", "interaction", { id });
        assert(attached.pid === pid && attached.runId === runId && live.conversationId === conversationId, "same process and bound conversation");
        assert.equal(await input.inputValue(), draft, "answer preserves the separate composer draft");
      }
      await api("terminal", "stop", { id, runId });
      console.log("PASS " + provider + " Chat approval for once, scoped consent and denial with exact PTY input and guarded target changes");
    }
  } finally {
    await stop();
  }
};

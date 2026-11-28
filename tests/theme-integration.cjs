"use strict";
const assert = require("node:assert/strict");
const { randomUUID } = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

module.exports = async function testTheme({ start, stop, api, until, sleep, page, pages, temporary, data }) {
  const screenshots = process.env.WORKFLOW_THEME_SCREENSHOTS;
  if (screenshots) fs.mkdirSync(screenshots, { recursive: true });
  const capture = async name => { if (screenshots) await page().screenshot({ path: path.join(screenshots, name + ".png") }); };
  const theme = () => page().evaluate(() => document.documentElement.dataset.theme);
  const setTheme = value => page().evaluate(value => window.workflowTheme.setPreference(value), value);
  const geometry = async () => {
    const positions = await page().evaluate(() => [".project-rail", ".sidebar", "main", ".inspector"].map(selector => {
      const r = document.querySelector(selector).getBoundingClientRect();
      return { x: r.x, width: r.width, height: r.height };
    }));
    for (let i = 1; i < positions.length; i++) assert(positions[i].x >= positions[i - 1].x + positions[i - 1].width, "panels remain horizontal without overlap");
    assert(positions[1].width >= 180 && positions[2].width >= 400 && positions[3].width >= 310, "unresized three-panel layout preserves usable widths");
    assert(positions.every(position => position.height > 400));
  };
  const appearance = async () => {
    await page().locator("#app-settings").click();
    await page().locator("#dialog-body").getByRole("button", { name: /^(Appearance|Görünüm)$/ }).click();
  };
  await start();
  try {
    await page().evaluate(() => localStorage.setItem("workflow-language", "tr"));
    await page().reload();
    await until(() => page().evaluate(() => !!window.workflow), "localized UI");
    const skip = page().locator("#dialog").getByRole("button", { name: /^(Skip|Atla)$/ });
    if (await skip.isVisible()) await skip.click();
    assert.equal(await theme(), "dark");
    await geometry();
    await appearance();
    await page().locator('input[name="appearance"][value="light"]').check();
    assert.equal(await theme(), "light");
    await geometry();
    await page().emulateMedia({ forcedColors: "active" });
    assert.equal(await page().locator('input[name="appearance"][value="light"]').evaluate(node => getComputedStyle(node).opacity), "1");
    await page().emulateMedia({ forcedColors: "none" });
    await capture("appearance-light");
    await page().locator("#dialog-footer button").click();
    const folder = path.join(temporary, "theme-project");
    fs.mkdirSync(folder);
    const sample = path.join(folder, "sample.js");
    fs.writeFileSync(sample, "export const layout = {\n  spacing: 8,\n  readable: false,\n};\n");
    for (const marker of ["chat-terminal-fixture", "chat-interaction-fixture"]) fs.writeFileSync(path.join(folder, marker), "");
    const project = await api("app", "add_project", { directory: folder });
    for (const provider of ["codex", "claude", "copilot"]) {
      const session = await api("app", "create_session", { projectId: project.id, provider, workspaceMode: "direct", tools: false });
      const { id } = session, conversationId = randomUUID(), timestamp = new Date().toISOString(), cwd = folder;
      const [location, header] = {
        codex: [["test-codex-home", "sessions", "rollout-" + conversationId + ".jsonl"], { type: "session_meta", payload: { id: conversationId, cwd, timestamp } }],
        claude: [["test-claude-home", "projects", "synthetic", conversationId + ".jsonl"], { type: "user", sessionId: conversationId, cwd, timestamp, message: { role: "user", content: "Review the sample layout." } }],
        copilot: [["test-copilot-home", "session-state", conversationId, "events.jsonl"], { type: "session.start", timestamp, data: { sessionId: conversationId, context: { cwd } } }],
      }[provider];
      const history = path.join(data, ...location);
      fs.mkdirSync(path.dirname(history), { recursive: true }); fs.writeFileSync(history, JSON.stringify(header) + "\n");
      await api("history", "bind", { id, conversationId });
      await page().evaluate(async id => { await window.workflow.refresh(); window.workflow.selectSession(id, true); }, id);
      await page().locator("#session-chat-view").click();
      await page().locator("#chat-start").click();
      await until(async () => (await api("chat", "interaction", { id })).ticket, provider + " ready");
      const run = await api("terminal", "attach", { id });
      await page().locator("#chat-input").fill("Review the sample layout.");
      const before = Buffer.from(run.data, "base64");
      for (const value of ["dark", "light"]) {
        await setTheme(value);
        const current = await api("terminal", "attach", { id });
        assert.equal(current.runId, run.runId); assert.equal(current.pid, run.pid);
        assert(Buffer.from(current.data, "base64").equals(before), "theme must not send provider input or clear its screen");
        assert.equal(await page().locator("#chat-input").inputValue(), "Review the sample layout.");
        assert.equal((await api("app", "state")).sessions.find(s => s.id === id).conversationId, conversationId);
        assert.equal(await page().evaluate(id => window.workflow.views.get(id).term.options.theme.background, id), value === "light" ? "#ffffff" : "#0c1015");
      }
      await page().locator("#chat-send").click();
      const approval = await until(async () => {
        const state = await api("chat", "interaction", { id });
        return state.terminalPrompt?.kind === "approval" && state.terminalPrompt.menu && state.terminalPrompt;
      }, provider + " approval");
      await page().locator("#chat-terminal-prompt").waitFor();
      await setTheme("dark"); await setTheme("light");
      const unchanged = await until(async () => {
        const prompt = (await api("chat", "interaction", { id })).terminalPrompt;
        return prompt?.menu && prompt;
      }, "actionable approval");
      assert.equal(unchanged.menu.question, approval.menu.question);
      await page().locator("#chat-terminal-prompt").getByRole("button", { name: /^1 ·/ }).click();
      await until(async () => (await api("chat", "interaction", { id })).terminalPrompt?.kind === "question", provider + " question");
      await page().locator("#chat-terminal-prompt").getByText("Apply the change where?", { exact: true }).waitFor();
      await page().locator("#chat-terminal-prompt").getByRole("radio", { name: /Only one/ }).waitFor();
      await capture(provider + "-light-chat");
      await page().locator("#session-terminal-view").click();
      await capture(provider + "-light-terminal");
      await setTheme("dark");
      await capture(provider + "-dark-terminal");
      await api("terminal", "stop", { id });
      await setTheme("light");
      console.log("PASS theme: " + provider + " live palette, draft, conversation, approval and question continuity");
    }
    fs.writeFileSync(sample, "export const layout = {\n  spacing: 12,\n  readable: true,\n};\n");
    await page().evaluate(() => window.workflow.openPanel("changes"));
    await page().locator(".diff-line.insert").first().waitFor();
    await page().locator(".diff-line.delete").first().waitFor();
    const diffColors = async () => page().evaluate(() => [".diff-line.insert", ".diff-line.delete"].map(selector => {
      const style = getComputedStyle(document.querySelector(selector));
      return [style.color, style.backgroundColor];
    }));
    assert.deepEqual(await diffColors(), [["rgb(40, 94, 59)", "rgb(232, 244, 233)"], ["rgb(137, 59, 61)", "rgb(250, 236, 235)"]]);
    await capture("light-changes");
    await setTheme("dark");
    assert.deepEqual(await diffColors(), [["rgb(192, 229, 172)", "rgb(20, 42, 32)"], ["rgb(240, 180, 191)", "rgb(50, 29, 39)"]]);
    await page().emulateMedia({ colorScheme: "dark" });
    await setTheme("system"); assert.equal(await theme(), "dark");
    await page().emulateMedia({ colorScheme: "light" });
    await until(async () => (await theme()) === "light", "system light");
    await setTheme("dark");
    await page().emulateMedia({ colorScheme: "dark" });
    await page().emulateMedia({ colorScheme: "light" }); await sleep(100);
    assert.equal(await theme(), "dark", "manual theme ignores OS changes");
    const popup = await until(() => pages().find(p => /\/notification\.html$/.test(p.url())), "actual notification WebView");
    await until(async () => await popup.evaluate(() => document.documentElement.dataset.theme === "dark"), "notification dark");
    assert.equal(await popup.evaluate(() => document.documentElement.dataset.theme), "dark");
    await setTheme("light");
    await until(async () => await popup.evaluate(() => document.documentElement.dataset.theme === "light"), "notification follows theme");
    assert.equal(await popup.locator("article").evaluate(node => getComputedStyle(node).backgroundColor), "rgb(239, 243, 237)");
    await stop(); await start();
    assert.equal(await theme(), "light", "theme survives application restart");
    assert.equal((await api("app", "state")).active.length, 0, "reopening does not restart providers");
    // Include the account-login terminal; it shares the same palette and cleanup.
    await page().evaluate(() => openProviderLogin("codex"));
    await page().locator(".login-terminal .xterm").waitFor();
    assert.equal(await page().locator(".login-terminal .xterm-viewport").evaluate(node => getComputedStyle(node).backgroundColor), "rgb(255, 255, 255)");
    await setTheme("dark");
    await until(async () => await page().locator(".login-terminal .xterm-viewport").evaluate(node => getComputedStyle(node).backgroundColor === "rgb(12, 16, 21)"), "login palette updates");
    await page().locator("#dialog-footer button").click();
    console.log("PASS theme: appearance controls, OS preference, persistence, notification sync and login terminal");
  } finally { await stop(); }
};

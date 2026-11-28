"use strict";

const assert = require("node:assert/strict");
const { randomUUID } = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

function bindFixtureConversation(data, folder, provider, conversationId) {
  const timestamp = new Date().toISOString();
  const files = {
    codex: ["test-codex-home", "sessions", "rollout-" + conversationId + ".jsonl"],
    claude: ["test-claude-home", "projects", "delivery", conversationId + ".jsonl"],
    copilot: ["test-copilot-home", "session-state", conversationId, "events.jsonl"],
  };
  const headers = {
    codex: { type: "session_meta", payload: { id: conversationId, cwd: folder, timestamp } },
    claude: { type: "user", sessionId: conversationId, cwd: folder, timestamp, message: { role: "user", content: "Test the sample." } },
    copilot: { type: "session.start", timestamp, data: { sessionId: conversationId, context: { cwd: folder } } },
  };
  const file = path.join(data, ...files[provider]);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(headers[provider]) + "\n");
}

module.exports = async function testVerificationDelivery({ start, stop, api, mcp, until, temporary, data }) {
  await start();
  const sessions = [];
  const inputDirectory = path.join(data, "test-input");
  fs.mkdirSync(inputDirectory, { recursive: true });
  try {
    for (const provider of ["codex", "claude", "copilot"]) {
      const folder = path.join(temporary, provider + "-delivery-project");
      fs.mkdirSync(folder);
      for (const name of ["chat-terminal-fixture", "verification-terminal-fixture", "verification-delivery-fixture"]) {
        fs.writeFileSync(path.join(folder, name), "");
      }
      const project = await api("app", "add_project", { directory: folder });
      const session = await api("app", "create_session", { projectId: project.id, provider, workspaceMode: "direct", tools: true });
      const conversationId = randomUUID();
      bindFixtureConversation(data, folder, provider, conversationId);
      await api("history", "bind", { id: session.id, conversationId });
      const control = path.join(inputDirectory, provider + "-delivery-mode");
      fs.writeFileSync(control, "normal");
      const terminal = await api("terminal", "start", { id: session.id });
      sessions.push({ ...session, folder, provider, control, conversationId, runId: terminal.runId });
    }

    for (const [index, session] of sessions.entries()) {
      const { id, provider, control, folder, runId, conversationId } = session;
      const state = () => api("verification", "state", { id });
      const act = async action => api("verification", action, { id, revision: (await state()).revision });
      // A deliberately concurrent action can settle while the test inspects the PTY.
      // Attach a rejection handler immediately and surface the error at the caller's await.
      const begin = action => {
        const pending = act(action);
        pending.catch(() => {});
        return pending;
      };
      const ready = () => until(async () => (await api("chat", "read", { id })).ticket, provider + " composer ready");
      const visible = (text, label) => until(async () => {
        const screen = await api("chat", "interaction", { id, terminalOutput: true });
        return screen.terminalOutput?.replace(/\s+/g, "").includes(text.replace(/\s+/g, ""));
      }, provider + " " + label);
      const type = data => api("terminal", "write", { id, runId, data });
      const wireFile = path.join(inputDirectory, provider + "-delivery-wire.bin");
      const wire = () => fs.existsSync(wireFile) ? fs.readFileSync(wireFile, "utf8") : "";
      const sent = (offset, enters) => {
        const bytes = wire().slice(offset);
        assert.equal(bytes.split("\x1b[200~").length - 1, 1, "one paste");
        assert.equal(bytes.split("\x1b[201~").length - 1, 1, "one complete paste");
        assert.equal(bytes.split("\r").length - 1, enters, "exact Enter count");
      };
      const pastedOperation = offset => until(() => {
        const bytes = wire().slice(offset);
        return bytes.includes("\x1b[201~") && /\boperation ([\da-f-]{36})\b/i.exec(bytes)?.[1];
      }, provider + " complete verification paste");
      const checkBinding = async () => {
        const attached = await api("terminal", "attach", { id });
        const saved = (await api("app", "state")).sessions.find(row => row.id === id);
        assert(attached.runId === runId && saved.conversationId === conversationId, "same process and bound conversation");
      };
      const request = operation => mcp(id, "verification_request", { operation });
      const items = [{ title: "Check sample output", check: "Read the sample and compare its output." }];
      await ready();

      // A draft already present before Prepare never becomes an authorized operation.
      await type("Existing draft");
      await until(async () => !(await api("chat", "read", { id })).ticket, "existing draft visible");
      const preflight = wire();
      await assert.rejects(act("prepare"), /hazır değil/);
      assert.equal((await state()).phase, "idle");
      assert(wire() === preflight, "preflight writes nothing to the PTY");
      await type("\r");
      await ready();

      // The provider consumes the paste directly. Its authenticated getter arrives
      // before the echo waiter finishes. No application-generated Enter is needed.
      fs.writeFileSync(control, "consume");
      let offset = wire().length;
      const early = begin("prepare");
      const earlyOperation = await pastedOperation(offset);
      assert.equal((await state()).delivery, "sending", "getter precedes the sender's observation deadline");
      const preparation = await request(earlyOperation);
      assert.equal(preparation.result?.phase, "preparing");
      assert.match(preparation.result.instructions, /Do NOT run checks/);
      assert((await request(randomUUID())).error, "wrong operation rejected");
      const other = sessions[(index + 1) % sessions.length];
      assert((await mcp(other.id, "verification_request", { operation: earlyOperation })).error, "wrong live session rejected");
      const acknowledged = await early;
      assert.equal(acknowledged.delivery, "received");
      assert.equal((await state()).delivery, "received");
      assert((await mcp(id, "verification_report", { operation: earlyOperation, results: [] })).error);
      await assert.rejects(act("start"));
      assert.equal((await mcp(id, "verification_propose", { operation: earlyOperation, items })).result?.phase, "ready");
      await checkBinding();
      sent(offset, 0);

      // Start is separately authorized. Keep the draft invisible past the sender
      // deadline, then render it and let a single manual Enter submit that draft.
      fs.writeFileSync(control, "delayed");
      offset = wire().length;
      const delayed = begin("start");
      const delayedOperation = await pastedOperation(offset);
      const unconfirmed = await delayed;
      assert.equal(unconfirmed.phase, "running");
      assert.equal(unconfirmed.delivery, "unconfirmed");
      assert(unconfirmed.operation === delayedOperation, "original operation retained");
      sent(offset, 0);
      fs.writeFileSync(control, "normal");
      await visible(delayedOperation, "delayed draft displayed");
      await type("\r");
      await until(() => wire().slice(offset).endsWith("\r"), "manual Enter received by fixture");
      const execution = await request(delayedOperation);
      assert.equal(execution.result?.phase, "running");
      assert.equal(execution.result.items.length, 1);
      assert.equal((await state()).delivery, "received");
      assert.equal((await request(delayedOperation)).result?.phase, "running", "repeat getter is idempotent");
      const results = execution.result.items.map(item => ({ id: item.id, status: "passed", evidence: "Controlled fixture received the approved sample check." }));
      assert.equal((await mcp(id, "verification_report", { operation: delayedOperation, results })).result?.phase, "reported");
      await checkBinding();
      sent(offset, 1);
      await ready();

      // Raw Enter can also win the window between paste and automatic confirmation.
      fs.writeFileSync(control, "hidden");
      offset = wire().length;
      const manual = begin("prepare");
      const manualOperation = await pastedOperation(offset);
      const cancelRevision = (await state()).revision;
      await type("\r");
      await until(() => wire().slice(offset).endsWith("\r"), "hidden draft submitted once");
      assert.equal((await request(manualOperation)).result?.phase, "preparing");
      assert.equal((await manual).phase, "preparing");
      assert.equal((await state()).delivery, "received");
      await api("verification", "cancel", { id, revision: cancelRevision });
      assert((await request(manualOperation)).error);
      assert.equal((await state()).reason, "cancelled");
      sent(offset, 1);
      await ready();

      // Editing the application's draft changes its provenance. Enter now sends an
      // ordinary development request, which must revoke the old operation.
      for (const mode of ["hidden", "delayed"]) {
        fs.writeFileSync(control, mode);
        offset = wire().length;
        const edited = begin("prepare");
        const editedOperation = await pastedOperation(offset);
        if (mode === "delayed") assert.equal((await edited).delivery, "unconfirmed");
        fs.writeFileSync(control, "normal");
        if (mode === "delayed") await visible(editedOperation, "verification draft visible before editing");
        const addition = " Add a separate sample change.";
        await type(addition);
        await visible(addition, "user edit displayed in composer");
        await type("\r");
        await edited;
        assert((await request(editedOperation)).error);
        assert.equal((await state()).reason, "newMessage");
        await until(() => wire().slice(offset).endsWith("\r"), "edited draft submitted");
        sent(offset, 1);
        await ready();
      }

      const source = await act("prepare");
      fs.writeFileSync(path.join(folder, "sample.txt"), "updated synthetic source");
      assert((await request(source.operation)).error);
      assert.equal((await state()).reason, "sourceChanged");

      await ready();
      const superseded = await act("prepare");
      const ticket = await ready();
      await api("chat", "send", { id, ...ticket, text: "Read the updated sample." });
      assert((await request(superseded.operation)).error);
      assert.equal((await state()).reason, "newMessage");
      await ready();

      const ending = await act("prepare");
      await checkBinding();
      await api("terminal", "stop", { id, runId });
      assert.equal((await state()).reason, "stopped");
      const next = await api("terminal", "start", { id });
      assert(next.runId !== runId, "restart creates a new run");
      await ready();
      assert((await request(ending.operation)).error, "old operation cannot cross a run boundary");
      const wrongRun = await mcp(id, "verification_request", { operation: ending.operation }, { runId });
      assert(!wrongRun.result, "old run cannot use a current lease");
      console.log("PASS " + provider + " verification delivery, manual Enter, authenticated receipt, conversation continuity and invalidation");
    }
  } finally {
    await stop();
  }
};

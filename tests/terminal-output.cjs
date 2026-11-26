const fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
const assert = require('node:assert/strict'), { test } = require('node:test');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const code = source.slice(source.indexOf('const TERMINAL_QUEUE_LIMIT ='), source.indexOf('function detectActivity('));
const b64 = text => Buffer.from(text).toString('base64');
const tick = () => new Promise(resolve => setImmediate(resolve));

function fixture(provider) {
  const views = new Map(), errors = [], writes = [], calls = [];
  const selected = { id: 'synthetic-session', provider };
  let read = async () => ({ runId: 'one', seq: 1000000, data: b64('current screen'), rows: 30, cols: 100 });
  const view = {
    card: { dataset: { id: selected.id } }, host: {}, overlay: {}, overlayText: {},
    ready: true, replaying: false, runId: 'one', seq: 0, attachGeneration: 0,
    queue: [], queuedBytes: 0, outputBusy: false, needsSnapshot: false, activityText: '',
    term: { reset() { writes.length = 0; }, resize() {}, write(bytes, done) {
      writes.push({ length: bytes.length, text: bytes.length < 100 ? Buffer.from(bytes).toString() : '' });
      setImmediate(done);
    } },
  };
  views.set(selected.id, view);
  const f = vm.runInNewContext(code + ';({attach,terminalPacket,queueTerminalPacket,drainTerminalPackets,limit:TERMINAL_QUEUE_LIMIT})', {
    views, session: () => selected, active: () => true, activity: new Map(),
    api: async (domain, action, args) => { calls.push({ domain, action, args }); return read(args); },
    atob: text => Buffer.from(text, 'base64').toString('binary'), Uint8Array, TextDecoder,
    setTimeout,
    tr: text => text, fitOne() {}, detectActivity() {}, sync: async () => {}, error: e => errors.push(e),
  });
  return { ...f, view, selected, views, writes, calls, errors, setRead(fn) { read = fn; },
    packet(seq, text = 'output', runId = 'one') { f.terminalPacket({ id: selected.id, seq, data: b64(text), runId }); },
    async idle() {
      for (let i = 0; i < 200; i++) {
        await tick();
        if (view.ready && !view.outputBusy && !view.queue.length && !view.needsSnapshot) return;
      }
      throw Error('terminal did not settle');
    },
  };
}

test('output flood remains bounded and recovers the current screen without provider input', async () => {
  for (const provider of ['codex', 'claude', 'copilot']) {
    const f = fixture(provider);
    for (let seq = 1; seq <= 1000; seq++) {
      f.packet(seq, 'x'.repeat(4096));
      assert(f.view.queuedBytes <= f.limit);
    }
    await f.idle();
    assert.equal(f.calls.length, 1);
    assert.deepEqual(JSON.parse(JSON.stringify(f.calls[0])), {
      domain: 'terminal', action: 'attach', args: { id: f.selected.id, screenOnly: true, subscribe: true },
    });
    assert.equal(f.view.seq, 1000000);
    assert.equal(f.view.queue.length, 0);
    assert.equal(f.view.queuedBytes, 0);
    assert.equal(f.writes.at(-1).text, 'current screen');
    assert.equal(f.errors.length, 0);
  }
});

test('ordinary output keeps its order and ignores obsolete runs and duplicated packets', async () => {
  const f = fixture('claude');
  f.packet(1, 'one'); f.packet(2, 'wrong', 'old'); f.packet(2, 'two'); f.packet(2, 'duplicate'); f.packet(3, 'three');
  await f.idle();
  assert.deepEqual(f.writes.map(w => w.text), ['one', 'two', 'three']);
  assert.equal(f.calls.length, 0);
});

test('replay writes use bounded chunks and a replaced view cannot finish an old attach', async () => {
  const f = fixture('copilot');
  f.setRead(async () => ({ runId: 'one', seq: 0, data: b64('x'.repeat(512 * 1024)), truncated: true }));
  await f.attach(f.view, f.selected);
  assert(f.writes.every(w => w.length <= 16 * 1024));
  assert.equal(f.writes.reduce((sum, w) => sum + w.length, 0), 512 * 1024);
  assert.match(f.view.host.title, /Tam geçmiş/);
  let reply;
  f.setRead(() => new Promise(resolve => { reply = resolve; }));
  const pending = f.attach(f.view, f.selected);
  await tick();
  f.views.delete(f.selected.id);
  reply({ runId: 'obsolete', seq: 0, data: b64('old') });
  await pending;
  assert.equal(f.view.runId, 'one');
});

test('a later attach wins even if an earlier response arrives last', async () => {
  const f = fixture('codex'), replies = [];
  f.setRead(() => new Promise(resolve => replies.push(resolve)));
  const older = f.attach(f.view, f.selected), newer = f.attach(f.view, f.selected);
  await tick();
  assert.equal(replies.length,1,"superseded subscription is skipped before its native call");
  replies[0]({ runId: 'new', seq: 20, data: b64('new') });
  await newer;
  await older;
  assert.equal(f.view.runId, 'new');
  assert.deepEqual(f.writes.map(w => w.text), ['new']);
});

test('an in-flight old attach finishes before a new native subscription can begin',async()=>{
  const f=fixture('codex'),replies=[];
  f.setRead(()=>new Promise(resolve=>replies.push(resolve)));
  const older=f.attach(f.view,f.selected); await tick();
  const newer=f.attach(f.view,f.selected); await tick();
  assert.equal(replies.length,1);
  replies[0]({runId:'old',seq:10,data:b64('old')}); await tick();
  assert.equal(replies.length,2);
  replies[1]({runId:'new',seq:20,data:b64('new')});
  await Promise.all([older,newer]);
  assert.equal(f.view.runId,'new'); assert.deepEqual(f.writes.map(w=>w.text),['new']);
});

test('packets arriving during replay are bounded and trigger recovery after parsing', async () => {
  const f = fixture('codex');
  let reads = 0;
  f.setRead(async () => ++reads === 1
    ? { runId: 'one', seq: 0, data: b64('x'.repeat(512 * 1024)) }
    : { runId: 'one', seq: 1000000, data: b64('latest prompt') });
  const pending = f.attach(f.view, f.selected);
  await tick();
  for (let seq = 1; seq <= 1000; seq++) f.packet(seq, 'x'.repeat(4096));
  assert(f.view.queuedBytes <= f.limit);
  await pending; await f.idle();
  assert.equal(reads, 2);
  assert.equal(f.writes.at(-1).text, 'latest prompt');
});

test('failed recovery keeps input blocked and later packets cannot build an unbounded queue', async () => {
  const f = fixture('claude');
  f.setRead(async () => { throw Error('synthetic read error'); });
  await assert.rejects(f.attach(f.view, f.selected, true));
  for (let seq = 1; seq <= 1000; seq++) f.packet(seq, 'x'.repeat(4096));
  assert.equal(f.view.ready, false);
  assert.equal(f.view.replaying, true);
  assert(f.view.queuedBytes <= f.limit);
});

const test = require('node:test'), assert = require('node:assert/strict'), fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const start = source.indexOf('function createUsageQueue('), end = source.indexOf('const scheduleUsage =', start);
const createQueue = vm.runInNewContext(source.slice(start, end) + '\ncreateUsageQueue');
const tick = () => new Promise(resolve => setImmediate(resolve));
test('quota refreshes share a four-request queue and every later account gets a turn', async () => {
  const schedule = createQueue(), started = [], release = [];
  const tasks = Array.from({ length: 7 }, (_, id) => schedule(() => { started.push(id); return new Promise(resolve => { release[id] = () => resolve(id); }); }));
  await tick(); assert.deepEqual(started, [0,1,2,3]);
  release[0](); await tick(); assert.deepEqual(started, [0,1,2,3,4]);
  release[1](); release[2](); await tick(); assert.deepEqual(started, [0,1,2,3,4,5,6]);
  for (const id of [3,4,5,6]) release[id]();
  assert.deepEqual(await Promise.all(tasks), [0,1,2,3,4,5,6]);
});
test('a failed account query releases its slot instead of starving later accounts', async () => {
  const schedule = createQueue(1), started = [];
  const failed = schedule(() => { started.push('failed'); throw Error('synthetic failure'); }).catch(() => null);
  const next = schedule(() => { started.push('next'); return 'ready'; });
  await failed; assert.equal(await next, 'ready'); assert.deepEqual(started, ['failed','next']);
});

import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, chmodSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';
import { callController } from './ipc.mjs';

async function fixture(handler, body) {
  const dir = mkdtempSync(join(tmpdir(), 'agent-tunnel-pi-'));
  const socket = join(dir, 'controller.sock');
  const server = createServer(handler);
  try {
    await new Promise((resolve, reject) => { server.once('error', reject); server.listen(socket, resolve); });
    chmodSync(socket, 0o600);
    await body(socket, dir);
  } finally {
    await new Promise(resolve => server.close(resolve));
    rmSync(dir, { recursive: true, force: true });
  }
}
test('reads a bounded correlated IPC reply', async () => {
  await fixture(stream => stream.once('data', data => {
    const request = JSON.parse(data.toString());
    assert.equal(request.op, 'info');
    stream.end(JSON.stringify({ id: request.id, result: { name: 'test' }, error: null }) + '\n');
  }), async socket => assert.equal((await callController(socket, { id: 'one', op: 'info' })).result.name, 'test'));
});
test('a dropped response is uncertain and never automatically retried', async () => {
  let requests = 0;
  await fixture(stream => stream.once('data', () => { requests++; stream.end(); }), async socket => {
    await assert.rejects(callController(socket, { id: 'side-effect', op: 'exec' }), /execution may have occurred/);
    assert.equal(requests, 1);
  });
});
test('rejects mismatched IDs', async () => {
  await fixture(stream => stream.once('data', () => stream.end('{"id":"wrong","result":{},"error":null}\n')), async socket => {
    await assert.rejects(callController(socket, { id: 'right', op: 'info' }), /Invalid controller reply/);
  });
});
test('fails closed for public directories and abort before send', async () => {
  await fixture(() => assert.fail('must not connect'), async (socket, dir) => {
    const abort = new AbortController(); abort.abort();
    await assert.rejects(callController(socket, { id: 'one', op: 'info' }, abort.signal), /Cancelled before sending/);
    chmodSync(dir, 0o755);
    assert.throws(() => callController(socket, { id: 'one', op: 'info' }), /must be private/);
  });
});
test('rejects invalid IDs and oversized requests before sending', async () => {
  await fixture(() => assert.fail('must not connect'), async socket => {
    assert.throws(() => callController(socket, { id: '../oops', op: 'info' }), /Invalid request ID/);
    assert.throws(() => callController(socket, { id: 'one', op: 'write', data: 'x'.repeat(256 * 1024) }), /exceeds size limit/);
  });
});

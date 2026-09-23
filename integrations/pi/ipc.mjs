import { connect } from 'node:net';
import { lstatSync } from 'node:fs';
import { dirname, resolve } from 'node:path';

const MAX = 256 * 1024;
export function callController(socketPath, request, signal) {
  if (!socketPath) throw new Error('Set AGENT_TUNNEL_SOCKET to a private Local Controller socket before starting pi.');
  const path = resolve(socketPath);
  const parent = lstatSync(dirname(path));
  const sock = lstatSync(path);
  if (!parent.isDirectory() || parent.isSymbolicLink() || (parent.mode & 0o077) || parent.uid !== process.getuid() ||
      !sock.isSocket() || sock.uid !== process.getuid() || (sock.mode & 0o077)) {
    throw new Error('Controller socket and directory must be private and owned by this user.');
  }
  if (!/^[A-Za-z0-9_-]{1,80}$/.test(request.id)) throw new Error('Invalid request ID.');
  const payload = Buffer.from(JSON.stringify(request) + '\n');
  if (payload.length > MAX) throw new Error('Controller request exceeds size limit.');
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(new Error('Cancelled before sending.')); return; }
    const socket = connect(path);
    let data = Buffer.alloc(0), settled = false, sent = false;
    const finish = (error, reply) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      signal?.removeEventListener('abort', abort);
      socket.destroy();
      error ? reject(error) : resolve(reply);
    };
    const uncertain = (reason) => new Error(`${reason}; request_id=${request.id}; ${sent ? 'execution may have occurred: keep the SAME ID and arguments, never auto-replay with a new ID' : 'request not sent'}.`);
    const abort = () => finish(uncertain('Call cancelled (this does not cancel an already-running remote job)'));
    const timer = setTimeout(() => finish(uncertain('Controller timeout')), 15_000);
    signal?.addEventListener('abort', abort, { once: true });
    socket.on('connect', () => { sent = true; socket.write(payload); });
    socket.on('error', () => finish(uncertain('Controller IPC failure')));
    socket.on('end', () => { if (!settled) finish(uncertain('Controller closed before a complete reply')); });
    socket.on('data', (chunk) => {
      if (data.length + chunk.length > MAX) { finish(uncertain('Oversized controller reply')); return; }
      data = Buffer.concat([data, chunk]);
      const newline = data.indexOf(10);
      if (newline < 0) return;
      try {
        const reply = JSON.parse(data.subarray(0, newline).toString('utf8'));
        if (reply.id !== request.id || typeof reply !== 'object') throw new Error('Mismatched response');
        finish(null, reply);
      } catch { finish(uncertain('Invalid controller reply')); }
    });
  });
}

// Tiny mitm proxy that logs every request's headers + status before
// forwarding to a real upstream. Used by the probe-anthropic-betas skill
// (step 2) to capture what `anthropic-beta` header a real client is sending.
//
// Usage:
//   node scripts/mitm-anthropic.mjs                       # listens on 8901, forwards to http://127.0.0.1:8900
//   MITM_PORT=8902 MITM_UPSTREAM=http://acr:8900 node ... # override
//
// Then point the client at http://localhost:8901/anthropic/... and inspect
// /tmp/mitm-cap.log.

import http from 'node:http';
import fs from 'node:fs';

const PORT = Number(process.env.MITM_PORT || 8901);
const UPSTREAM = process.env.MITM_UPSTREAM || 'http://127.0.0.1:8900';
const LOG = process.env.MITM_LOG || '/tmp/mitm-cap.log';

const out = fs.createWriteStream(LOG, { flags: 'a' });
const log = (...args) => {
  const line = args.map(a => typeof a === 'string' ? a : JSON.stringify(a, null, 2)).join(' ');
  out.write(line + '\n');
  console.error(line);
};

http.createServer(async (req, res) => {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const body = Buffer.concat(chunks);
  log('[mitm]', req.method, req.url);
  log('[mitm] headers:', req.headers);
  const beta = req.headers['anthropic-beta'];
  if (beta) log('[mitm] !!! anthropic-beta:', beta);
  try {
    const upstream = await fetch(UPSTREAM + req.url, {
      method: req.method,
      headers: req.headers,
      body: body.length ? body : undefined,
      redirect: 'manual',
    });
    res.writeHead(upstream.status, Object.fromEntries(upstream.headers));
    const buf = Buffer.from(await upstream.arrayBuffer());
    res.end(buf);
    log('[mitm] upstream', upstream.status);
    if (upstream.status >= 400) log('[mitm] body:', buf.toString('utf8').slice(0, 400));
  } catch (e) {
    log('[mitm] error:', e.message);
    res.writeHead(502); res.end(e.message);
  }
}).listen(PORT, '127.0.0.1', () => log(`mitm listening ${PORT} -> ${UPSTREAM}`));

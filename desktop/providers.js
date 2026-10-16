// Credential-bearing protocol stays in the main process. Renderer requests
// cannot import/export tokens or choose an authorization URL.
const { createHash } = require('node:crypto');

function createProviders({ spawn, openExternal }) {
  let login;
  function cancel() { login?.abort(); login = undefined; }
  function call(target, request, signal) {
    return new Promise((resolve, reject) => {
      const child = spawn(target);
      let buffer = '', stderr = '', result, settled = false, authorizing = false;
      const timeout = setTimeout(() => finish(new Error('Provider setup timed out. Try again.')), request.action === 'login' ? 660000 : 60000);
      function finish(error) {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        signal?.removeEventListener('abort', abort);
        if (error) { child.kill(); reject(error); } else resolve(result);
      }
      const abort = () => finish(new Error('Sign-in cancelled.'));
      signal?.addEventListener('abort', abort, { once: true });
      if (signal?.aborted) { abort(); return; }
      child.on('error', () => finish(new Error('Could not start SUI provider setup. Install or update SUI on this host.')));
      child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4000); });
      child.stdout.on('data', chunk => {
        if (settled) return;
        buffer += chunk;
        if (buffer.length > 256 * 1024) { finish(new Error('Provider response exceeds 256 KiB.')); return; }
        let end;
        while ((end = buffer.indexOf('\n')) >= 0) {
          const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
          let frame;
          try { frame = JSON.parse(line); } catch { finish(new Error('Invalid provider response.')); return; }
          if (frame.type === 'authorize') {
            if (request.action !== 'login' || target.kind !== 'local' || authorizing) { finish(new Error('Unexpected authorization request.')); return; }
            let url;
            try {
              url = new URL(frame.url);
              if (url.origin !== 'https://auth.openai.com' || url.pathname !== '/api/accounts/authorize' || url.username || url.password) throw new Error();
            } catch { finish(new Error('Untrusted authorization URL.')); return; }
            authorizing = true;
            // Never log this URL or emit it to the renderer: returning sign-in
            // can contain a retained ID token as an authorization hint.
            Promise.resolve(openExternal(url.toString())).catch(() => finish(new Error('Could not open the sign-in browser.')));
          } else if (result) { finish(new Error('Unexpected extra provider result.')); return; }
          else result = frame;
        }
      });
      child.on('close', code => {
        if (code !== 0) finish(new Error(stderr.trim() || 'SUI provider setup failed. Install or update SUI on this host.'));
        else if (!result || buffer.trim()) finish(new Error('Incomplete provider response.'));
        else finish();
      });
      child.stdin.on('error', () => finish(new Error('Provider connection closed.')));
      const line = JSON.stringify(request);
      if (Buffer.byteLength(line) > 128 * 1024) { finish(new Error('Provider request exceeds 128 KiB.')); return; }
      child.stdin.end(line + '\n');
    });
  }
  async function request(target, input) {
    if (!['profiles', 'models', 'login', 'select', 'logout'].includes(input?.action)) throw new Error('Unsupported provider action.');
    const action = input.action;
    const name = String(input.name || '');
    if (action !== 'profiles' && !/^[A-Za-z0-9_-]{1,64}$/.test(name)) throw new Error('Choose a profile name using letters, digits, hyphens or underscores.');
    if (action === 'login') {
      cancel();
      const controller = new AbortController(); login = controller;
      try {
        let result;
        if (target.kind === 'ssh') {
          const registration = await call(target, { action: 'registration', name }, controller.signal);
          // Stable per destination/profile: retrying an expired code reuses
          // the issued registration retained before the failed exchange.
          const temporary = 'transfer-' + createHash('sha256').update(JSON.stringify([target.host, name])).digest('hex').slice(0, 40);
          const transfer = await call({ kind: 'local' }, { action, name: temporary, transfer: true, registration: registration.record }, controller.signal);
          if (transfer.type !== 'transfer') throw new Error('ChatGPT credential transfer did not complete.');
          result = await call(target, { action: 'import', name, record: transfer.record }, controller.signal);
        } else result = await call(target, { action, name }, controller.signal);
        return publicResult(result);
      } finally { if (login === controller) login = undefined; }
    }
    return publicResult(await call(target, { action, name, ...(action === 'select' ? { model: String(input.model || '') } : {}) }));
  }
  return { request, cancel };
}

function publicResult(result) {
  if (result.type === 'profiles') return { type: 'profiles', truncated: !!result.truncated, items: (result.items || []).slice(0, 200).map(p => ({ name: p.name, model: p.model, kind: p.kind, account: p.account ? accountSummary(p.account) : null })) };
  if (result.type === 'models') return { type: 'models', truncated: !!result.truncated, items: (result.items || []).slice(0, 200).map(m => ({ id: m.id, name: m.name })) };
  if (result.type === 'account') return { type: 'account', account: accountSummary(result.account), revocation_confirmed: result.revocation_confirmed };
  if (result.type === 'profile_saved') return { type: 'profile_saved', name: result.name };
  throw new Error('Unexpected provider result.');
}
function accountSummary(account) {
  return { connected: !!account?.connected, sharing: !!account?.sharing, email: String(account?.email || '').slice(0, 200) };
}
module.exports = { createProviders };

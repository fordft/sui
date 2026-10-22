// Suggestions only: recovery never resubmits a turn or grants permissions.
(function (root) {
  function actions(message, { connected = false, chatgpt = false, ssh = false } = {}) {
    const text = String(message).toLowerCase();
    if (/401|unauthori[sz]ed|invalid[_ -]?(api[_ -]?)?key|no api key|token.*expir|sign.?in|authentication|connect this chatgpt/.test(text)) return [{ id: 'configure', label: chatgpt ? 'Sign in again' : 'Configure provider' }];
    if (/quota|rate.?limit|429|subscription_sharing_usage_|usage limit/.test(text)) return [...(chatgpt ? [{ id: 'usage', label: 'Manage usage' }] : []), { id: connected ? 'model' : 'configure', label: 'Choose another model or provider' }];
    if (/older.*sui|update sui|desktop.protocol/.test(text) && ssh) return [{ id: 'connection', label: 'Update SUI on SSH host' }];
    if (/model|reasoning|400|invalid.request|configuration|profile/.test(text)) return [{ id: connected ? 'model' : 'configure', label: connected ? 'Change model or reasoning' : 'Review provider settings' }];
    if (!connected || /disconnect|timed? ?out|econn|connection|network|closed/.test(text)) return [{ id: 'reconnect', label: 'Reconnect to saved chat' }, { id: 'connection', label: 'Connection settings' }];
    return [{ id: 'connection', label: 'Connection settings' }];
  }
  if (typeof module !== 'undefined' && module.exports) module.exports = { actions };
  else root.SuiRecovery = { actions };
})(globalThis);

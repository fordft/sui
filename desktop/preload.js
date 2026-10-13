const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('sui', {
  projects: () => ipcRenderer.invoke('sui:projects'),
  connect: (item, resume) => ipcRenderer.invoke('sui:connect', item, resume),
  configure: (item, profile) => ipcRenderer.invoke('sui:configure', item, profile),
  installRemote: item => ipcRenderer.invoke('sui:install-remote', item),
  disconnect: () => ipcRenderer.invoke('sui:disconnect'),
  command: value => ipcRenderer.invoke('sui:command', value),
  openBrowser: url => ipcRenderer.invoke('sui:open-browser', url),
  previewRemote: port => ipcRenderer.invoke('sui:preview-remote', port),
  browserBounds: bounds => ipcRenderer.invoke('sui:browser-bounds', bounds),
  hideBrowser: () => ipcRenderer.invoke('sui:browser-hide'),
  back: () => ipcRenderer.invoke('sui:browser-back'),
  forward: () => ipcRenderer.invoke('sui:browser-forward'),
  reload: () => ipcRenderer.invoke('sui:browser-reload'),
  annotate: () => ipcRenderer.invoke('sui:annotate'),
  onEvent: callback => {
    const handler = (_event, value) => callback(value);
    ipcRenderer.on('sui:event', handler);
    return () => ipcRenderer.removeListener('sui:event', handler);
  }
});

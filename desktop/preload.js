const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('sui', {
  platform: process.platform,
  checkUpdate: () => ipcRenderer.invoke('sui:update-check'),
  downloadUpdate: () => ipcRenderer.invoke('sui:update-download'),
  projects: () => ipcRenderer.invoke('sui:projects'),
  sshHosts: () => ipcRenderer.invoke('sui:ssh-hosts'),
  pickWorkspace: () => ipcRenderer.invoke('sui:pick-workspace'),
  remoteFolders: (host, directory, hidden) => ipcRenderer.invoke('sui:remote-folders', host, directory, hidden),
  cancelFolderBrowse: () => ipcRenderer.invoke('sui:cancel-folder-browse'),
  connect: (item, resume) => ipcRenderer.invoke('sui:connect', item, resume),
  configure: (item, profile) => ipcRenderer.invoke('sui:configure', item, profile),
  installRemote: item => ipcRenderer.invoke('sui:install-remote', item),
  disconnect: () => ipcRenderer.invoke('sui:disconnect'),
  command: value => ipcRenderer.invoke('sui:command', value),
  openBrowser: url => ipcRenderer.invoke('sui:open-browser', url),
  previewRemote: port => ipcRenderer.invoke('sui:preview-remote', port),
  browserBounds: bounds => ipcRenderer.invoke('sui:browser-bounds', bounds),
  browserShow: () => ipcRenderer.invoke('sui:browser-show'),
  hideBrowser: () => ipcRenderer.invoke('sui:browser-hide'),
  back: () => ipcRenderer.invoke('sui:browser-back'),
  forward: () => ipcRenderer.invoke('sui:browser-forward'),
  reload: () => ipcRenderer.invoke('sui:browser-reload'),
  annotate: () => ipcRenderer.invoke('sui:annotate'),
  setChrome: theme => ipcRenderer.invoke('sui:chrome', theme),
  onEvent: callback => {
    const handler = (_event, value) => callback(value);
    ipcRenderer.on('sui:event', handler);
    return () => ipcRenderer.removeListener('sui:event', handler);
  }
});

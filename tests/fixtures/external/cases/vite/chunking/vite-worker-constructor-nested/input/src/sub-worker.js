import('./worker-extra.js').then((mod) => {
  self.postMessage(mod.extraValue);
});
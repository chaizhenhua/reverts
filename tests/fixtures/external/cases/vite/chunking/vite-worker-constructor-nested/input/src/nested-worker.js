import SubWorker from './sub-worker.js?worker';
const subWorker = new SubWorker();
subWorker.onmessage = (event) => {
  self.postMessage({ type: 'sub-worker', data: event.data });
};
import('./worker-module.js').then((mod) => {
  self.postMessage({ type: 'module', data: mod.moduleValue });
});
self.onmessage = () => {
  subWorker.postMessage('nested');
};
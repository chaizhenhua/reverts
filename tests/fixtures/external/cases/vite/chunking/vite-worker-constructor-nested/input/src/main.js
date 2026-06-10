import NestedWorker from './nested-worker.js?worker';
export function start() {
  const worker = new NestedWorker();
  worker.postMessage('ping');
  return worker;
}
start();
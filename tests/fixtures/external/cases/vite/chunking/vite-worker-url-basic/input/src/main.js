const worker = new Worker(new URL('./url-worker.js', import.meta.url), { type: 'module' });
worker.postMessage('main');
worker.addEventListener('message', (event) => console.log(event.data));
export function getWorker() {
  return worker;
}
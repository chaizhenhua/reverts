import { message } from './worker-shared.js';
self.onmessage = (event) => {
  self.postMessage({ type: 'url-worker', message, echo: event.data });
};
self.postMessage({ type: 'init', message });
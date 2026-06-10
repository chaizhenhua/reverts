import { sharedValue } from './shared.js';
const view = 'foo';
export async function loadFoo() {
  const { msg } = await import(`./views/${view}.js`);
  return msg + ':' + sharedValue;
}
export async function loadBar() {
  const { msg } = await import('./views/bar.js');
  return msg;
}
console.log(sharedValue);
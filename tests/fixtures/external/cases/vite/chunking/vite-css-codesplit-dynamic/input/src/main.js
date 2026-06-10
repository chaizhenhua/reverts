import './style.css';
import './chunk.css';
export async function loadAsync() {
  await import('./async-js.js');
  return import('./mod.module.css');
}
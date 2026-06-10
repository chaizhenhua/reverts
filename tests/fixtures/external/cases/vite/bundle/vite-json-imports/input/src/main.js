import json, { hello } from './test.json';
import raw from './test.json?raw';
import url from './test.json?url';
export { json, hello, raw, url };
import('./test.json').then((mod) => console.log(mod.default, mod.hello));
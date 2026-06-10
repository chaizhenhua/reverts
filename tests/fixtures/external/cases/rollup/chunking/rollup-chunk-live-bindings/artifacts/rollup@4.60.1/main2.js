import { a as fn, b as text } from './chunks/dep1.js';

class Main2 {
  constructor () {
    fn();
    console.log(text);
  }
}

export { Main2 as default };

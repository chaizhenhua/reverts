import {
  foo,
  setFoo
} from "./chunks/chunk.js";

// input/a.js
setFoo(123);
console.log(foo);

import {
  require_shared
} from "./chunks/chunk.js";

// input/a.js
var { foo } = require_shared();
console.log(foo);

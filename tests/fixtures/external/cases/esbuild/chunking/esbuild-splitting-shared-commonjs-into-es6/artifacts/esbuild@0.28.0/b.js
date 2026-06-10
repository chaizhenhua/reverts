import {
  require_shared
} from "./chunks/chunk.js";

// input/b.js
var { foo } = require_shared();
console.log(foo);

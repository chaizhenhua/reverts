import"./chunks/chunk-2.js";
import {
  exports_a
} from "./chunks/chunk-5.js";

// tests/fixtures/external/cases/bun/chunking/bun-splitting-circular-dynamic-imports-with-css/input/b.js
console.log("b.js executed");
var bValue = "B";
console.log("b.js imports a", exports_a);
export {
  bValue
};

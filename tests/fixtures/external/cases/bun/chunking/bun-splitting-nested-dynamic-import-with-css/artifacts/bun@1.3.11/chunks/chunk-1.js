import {
  __require
} from "./chunks/chunk-0.js";

// tests/fixtures/external/cases/bun/chunking/bun-splitting-nested-dynamic-import-with-css/input/level1.js
console.log("level1.js executed");
import("./chunks/chunk-3.js").then(() => console.log("level2 loaded from level1"));

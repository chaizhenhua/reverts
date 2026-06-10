import {
  __require
} from "./chunks/chunk-0.js";

// tests/fixtures/external/cases/bun/chunking/bun-splitting-nested-dynamic-import-with-css/input/entry.js
import("./chunks/chunk-1.js").then(() => console.log("level1 loaded"));

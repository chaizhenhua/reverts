import {
  __require
} from "./chunks/chunk-2.js";

// tests/fixtures/external/cases/bun/chunking/bun-splitting-circular-dynamic-imports-with-css/input/entry.js
import("./chunks/chunk-0.js").then((module) => {
  console.log("a loaded from entry");
  return import("./chunks/chunk-1.js");
}).then((module) => {
  console.log("b loaded from entry, value:", module.bValue);
});

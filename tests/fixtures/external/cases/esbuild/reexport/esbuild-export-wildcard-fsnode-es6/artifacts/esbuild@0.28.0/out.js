// input/entry.js
export * from "fs";

// input/internal.js
var foo = 123;

// input/entry.js
export * from "./external";
export {
  foo
};

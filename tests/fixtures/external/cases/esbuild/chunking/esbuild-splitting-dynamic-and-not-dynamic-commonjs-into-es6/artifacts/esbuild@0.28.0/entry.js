import {
  __toESM,
  require_foo
} from "./chunks/chunk.js";

// input/entry.js
var import_foo = __toESM(require_foo());
import("./chunks/foo.js").then(({ default: { bar: b } }) => console.log(import_foo.bar, b));

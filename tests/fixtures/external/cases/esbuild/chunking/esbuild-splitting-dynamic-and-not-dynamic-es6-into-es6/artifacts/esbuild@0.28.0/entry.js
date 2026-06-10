import {
  bar
} from "./chunks/chunk.js";

// input/entry.js
import("./chunks/foo.js").then(({ bar: b }) => console.log(bar, b));

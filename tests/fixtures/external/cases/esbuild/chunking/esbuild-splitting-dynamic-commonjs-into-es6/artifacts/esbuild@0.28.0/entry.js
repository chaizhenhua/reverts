// input/entry.js
import("./chunks/foo.js").then(({ default: { bar } }) => console.log(bar));

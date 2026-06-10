// @bun
// tests/fixtures/external/cases/bun/bundle/bun-chained-imports-basic/input/b.js
var b = "b";

// tests/fixtures/external/cases/bun/bundle/bun-chained-imports-basic/input/a.js
var a = "a" + b;

// tests/fixtures/external/cases/bun/bundle/bun-chained-imports-basic/input/entry.js
console.log(a);

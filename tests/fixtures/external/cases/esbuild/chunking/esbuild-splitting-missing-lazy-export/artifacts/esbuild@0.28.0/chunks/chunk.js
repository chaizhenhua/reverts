// empty.js
var empty_exports = {};

// common.js
function foo() {
  return [empty_exports, void 0];
}
function bar() {
  return [void 0];
}

export {
  foo,
  bar
};

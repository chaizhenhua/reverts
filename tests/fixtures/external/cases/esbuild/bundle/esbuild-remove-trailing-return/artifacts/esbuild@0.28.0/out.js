// input/entry.js
function foo() {
  a && b();
}
function bar() {
  return a && b(), KEEP_ME;
}
var entry_default = [
  foo,
  bar,
  function() {
    a && b();
  },
  function() {
    return a && b(), KEEP_ME;
  },
  () => {
    a && b();
  },
  () => (a && b(), KEEP_ME)
];
export {
  entry_default as default
};

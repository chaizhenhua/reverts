// circular-import-cycle.js
console.log(bar());

// circular-import-constants.js
var foo = 123;
function bar() {
  return foo;
}

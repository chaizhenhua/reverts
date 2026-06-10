// circular-re-export-star-cycle.js
console.log(bar());

// circular-re-export-star-constants.js
var foo = 123;
function bar() {
  return foo;
}

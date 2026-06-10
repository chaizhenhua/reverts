// circular-re-export-cycle.js
var baz = 0;
console.log(bar());

// circular-re-export-constants.js
var foo = 123;
function bar() {
  return foo;
}

// circular-re-export-entry.js
console.log(baz);

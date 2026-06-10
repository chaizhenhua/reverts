// cross-module-constants.js
foo();
var y_keep = 1;
function foo() {
  return [1, y_keep];
}

// cross-module-entry.js
console.log(1, y_keep);

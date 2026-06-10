// non-circular-export-constants.js
function bar() {
  return 123;
}

// non-circular-export-entry.js
console.log(123, bar());

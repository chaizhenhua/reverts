(() => {
  // input/Users/user/project/node_modules/d/index.js
  var foo = 123;

  // input/Users/user/project/node_modules/b/index.js
  throw "keep this";

  // input/Users/user/project/src/entry.js
  console.log(foo);
})();

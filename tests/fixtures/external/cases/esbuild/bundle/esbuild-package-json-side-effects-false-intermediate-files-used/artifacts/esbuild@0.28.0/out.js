(() => {
  // input/Users/user/project/node_modules/demo-pkg/foo.js
  var foo = 123;

  // input/Users/user/project/node_modules/demo-pkg/index.js
  throw "keep this";

  // input/Users/user/project/src/entry.js
  console.log(foo);
})();

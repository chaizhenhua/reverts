(() => {
  // input/Users/user/project/node_modules/d/index.js
  var foo = 123;

  // input/Users/user/project/node_modules/b1/index.js
  throw "keep this 1";

  // input/Users/user/project/node_modules/b2/index.js
  throw "keep this 2";

  // input/Users/user/project/src/entry.js
  console.log(foo);
})();

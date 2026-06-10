(() => {
  // input/Users/user/project/node_modules/pkg/index.js
  var foo = "foo";
  var bar = "bar";

  // input/Users/user/project/node_modules/js-pkg/index.js
  var foo2 = foo;

  // input/Users/user/project/node_modules/ts-pkg/index.ts
  var bar2 = bar;

  // input/Users/user/project/shim.ts
  var foo3 = "shimFoo";
  var bar3 = "shimBar";

  // input/Users/user/project/src/main.ts
  if (foo2 !== "foo") throw "fail: foo";
  if (bar2 !== "bar") throw "fail: bar";
  if (foo3 !== "shimFoo") throw "fail: shimFoo";
  if (bar3 !== "shimBar") throw "fail: shimBar";
})();

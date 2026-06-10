(() => {
  // Users/user/project/src/bar.js
  function bar() {
    throw new Error("test");
  }

  // Users/user/project/src/data.txt
  var data_default = "#2041";

  // Users/user/project/src/entry.js
  function foo() {
    bar();
  }
  foo();
  console.log(data_default);
})();
//# sourceMappingURL=out.js.map

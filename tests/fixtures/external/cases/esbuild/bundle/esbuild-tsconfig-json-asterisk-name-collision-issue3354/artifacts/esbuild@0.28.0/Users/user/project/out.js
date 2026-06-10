(() => {
  // web/bar/foo/foo.ts
  function foo() {
    console.log("bar/foo");
  }

  // web/foo.ts
  function foo2() {
    console.log("web/foo");
    foo();
  }

  // entry.ts
  foo2();
})();

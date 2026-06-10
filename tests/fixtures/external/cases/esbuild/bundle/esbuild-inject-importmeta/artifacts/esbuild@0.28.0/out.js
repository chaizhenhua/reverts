(() => {
  // inject.js
  var foo = 1;
  var bar = 2;
  var baz = 3;

  // entry.js
  console.log(
    // These should be fully substituted
    foo,
    bar,
    baz,
    // Should just substitute "import.meta.foo"
    bar.baz,
    // This should not be substituted
    foo.bar
  );
})();

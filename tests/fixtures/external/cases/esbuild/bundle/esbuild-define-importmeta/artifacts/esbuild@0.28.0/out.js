(() => {
  // entry.js
  var import_meta = {};
  console.log(
    // These should be fully substituted
    import_meta,
    import_meta.foo,
    import_meta.foo.bar,
    // Should just substitute "import.meta.foo"
    import_meta.foo.baz,
    // This should not be substituted
    import_meta.bar
  );
})();

(() => {
  // node_modules/alias1/index.js
  console.log(1);

  // node_modules/alias2/foo.js
  console.log(2);

  // node_modules/alias3/index.js
  console.log(3);

  // node_modules/alias4/index.js
  console.log(4);

  // node_modules/alias5/foo.js
  console.log(5);

  // alias6/dir/index.js
  console.log(6);

  // alias7/dir/foo/index.js
  console.log(7);

  // alias8/dir/pkg8/index.js
  console.log(8);

  // alias9/some/file.js
  console.log(9);

  // node_modules/prefix-foo/index.js
  console.log(10);

  // node_modules/@scope/prefix-foo/index.js
  console.log(11);
})();

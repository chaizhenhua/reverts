(() => {
  // input/foo.js
  function fn() {
    return 123;
  }

  // input/entry.js
  console.log(fn());
})();

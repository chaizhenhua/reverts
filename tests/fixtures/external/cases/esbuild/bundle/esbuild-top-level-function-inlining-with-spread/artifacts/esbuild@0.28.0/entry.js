(() => {
  // input/entry.js
  function identity1(x) {
    return x;
  }
  function identity3(x) {
    return x;
  }
  args;
  [...args];
  identity1();
  args;
  identity3(...args);
})();

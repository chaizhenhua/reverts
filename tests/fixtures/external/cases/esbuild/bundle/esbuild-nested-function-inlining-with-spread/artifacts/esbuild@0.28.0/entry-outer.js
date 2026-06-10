(() => {
  // input/inner.js
  function identity1(x) {
    return x;
  }
  function identity3(x) {
    return x;
  }

  // input/entry-outer.js
  check(
    void 0,
    (args, void 0),
    ([...args], void 0),
    identity1(),
    args,
    identity3(...args)
  );
})();

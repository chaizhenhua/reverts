(() => {
  // input/entry.js
  // @__NO_SIDE_EFFECTS__
  function foo() {
  }
  use(foo`keep`);
  keep, alsoKeep;
  `${keep}${alsoKeep}`;
})();

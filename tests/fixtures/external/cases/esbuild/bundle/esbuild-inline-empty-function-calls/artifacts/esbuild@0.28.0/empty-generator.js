(() => {
  // input/empty-generator.js
  function* keep() {
  }
  console.log(keep());
  keep(foo());
  keep(1);
})();

(() => {
  // input/empty-first.js
  function keep() {
    return x;
  }
  console.log(keep());
  keep(foo());
  keep(1);
})();

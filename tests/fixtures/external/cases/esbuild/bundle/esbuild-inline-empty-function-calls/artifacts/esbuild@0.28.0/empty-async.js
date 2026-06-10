(() => {
  // input/empty-async.js
  async function keep() {
  }
  console.log(keep());
  keep(foo());
  keep(1);
})();

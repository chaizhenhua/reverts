(() => {
  // input/not-identity-array.js
  function keep([x]) {
    return x;
  }
  console.log(keep(1));
  keep(foo());
  keep(1);
})();

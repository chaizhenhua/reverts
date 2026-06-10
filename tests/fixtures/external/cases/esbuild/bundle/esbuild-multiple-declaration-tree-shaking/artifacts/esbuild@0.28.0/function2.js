(() => {
  // input/function2.js
  function x() {
    return 1;
  }
  console.log(x());
  function x() {
    return 2;
  }
})();

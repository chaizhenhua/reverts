(() => {
  // input/Users/user/project/node_modules/fib/index.js
  function fib(input) {
    if (input < 2) {
      return input;
    }
    return fib(input - 1) + fib(input - 2);
  }

  // input/Users/user/project/entry.ts
  console.log(fib(10));
})();

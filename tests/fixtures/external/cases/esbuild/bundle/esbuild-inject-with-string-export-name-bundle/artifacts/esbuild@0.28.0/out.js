(() => {
  // inject.js
  var old = console.log;
  var fn = (...args) => old.apply(console, ["log:"].concat(args));

  // entry.js
  fn(test);
  fn(test);
  fn(test);
})();

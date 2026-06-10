(() => {
  // input/empty-comma.js
  console.log(foo());
  console.log((foo(), void 0));
  console.log((foo(), void 0));
  for (; void 0; ) ;
  foo();
  foo();
  foo();
})();

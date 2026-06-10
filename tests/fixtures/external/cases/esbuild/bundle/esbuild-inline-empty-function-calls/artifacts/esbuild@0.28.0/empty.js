(() => {
  // input/empty.js
  console.log((foo(), bar(), void 0));
  console.log((foo(), void 0));
  console.log((foo(), void 0));
  console.log(void 0);
  console.log(void 0);
  foo(), bar();
  foo();
  foo();
})();

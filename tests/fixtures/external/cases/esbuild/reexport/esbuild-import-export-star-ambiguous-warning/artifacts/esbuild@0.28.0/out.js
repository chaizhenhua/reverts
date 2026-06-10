(() => {
  // foo.js
  var x = 1;

  // bar.js
  var z = 4;

  // entry.js
  console.log(x, void 0, z);
})();

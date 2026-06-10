(() => {
  // input/data.json
  var data_default = { works: true };

  // input/data.json with { type: 'json' }
  var data_default2 = { works: true };

  // input/entry.js
  console.log(data_default === data_default, data_default !== data_default2);
})();

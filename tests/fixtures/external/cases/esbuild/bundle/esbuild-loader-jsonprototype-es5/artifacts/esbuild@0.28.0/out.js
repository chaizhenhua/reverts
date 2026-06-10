(function() {
  // input/data.json
  var data_default = {
    "": "The property below should NOT be converted to a computed property for ES5:",
    __proto__: { foo: "bar" }
  };

  // input/entry.js
  console.log(data_default);
})();

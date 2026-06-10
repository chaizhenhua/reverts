(() => {
  // input/data.json
  var data_default = {
    "": "The property below should be converted to a computed property:",
    ["__proto__"]: { foo: "bar" }
  };

  // input/entry.js
  console.log(data_default);
})();

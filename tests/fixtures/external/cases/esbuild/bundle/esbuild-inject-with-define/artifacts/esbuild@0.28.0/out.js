(() => {
  // inject.js
  var second = "success (identifier)";
  var second2 = "success (dot name)";

  // entry.js
  console.log(
    // define wins over inject
    true,
    true,
    // define forwards to inject
    second === "success (identifier)",
    second2 === "success (dot name)"
  );
})();

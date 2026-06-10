(() => {
  // keep-me/index.js
  console.log("side effects");

  // entry.jsx
  function KeepMe1() {
  }
  var keepMe2 = React.createElement(KeepMe1, null);
  function keepMe3() {
    console.log("side effects");
  }
  var keepMe4 = keepMe3();
  var keepMe5 = pure();
  var keepMe6 = some.fn();
})();

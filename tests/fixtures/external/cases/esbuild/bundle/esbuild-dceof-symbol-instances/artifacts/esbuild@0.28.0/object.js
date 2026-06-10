(() => {
  // input/object.js
  var keep1 = { *[Symbol.iterator]() {
  }, [keep]: null };
  var keep2 = { [keep]: null, *[Symbol.iterator]() {
  } };
  var keep3 = { *[Symbol.wtf]() {
  } };
})();

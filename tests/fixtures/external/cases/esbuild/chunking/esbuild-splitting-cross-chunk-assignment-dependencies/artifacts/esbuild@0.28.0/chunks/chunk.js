// input/shared.js
var observer;
var value;
function getValue() {
  return value;
}
function setValue(next) {
  value = next;
  if (observer) observer();
}
sideEffects(getValue);

export {
  setValue
};

// @__NO_SIDE_EFFECTS__
function f(y) {
  sideEffect(y);
}
// @__NO_SIDE_EFFECTS__
function* g(y) {
  sideEffect(y);
}
onlyKeepThisIdentifier;
onlyKeepThisIdentifier;
x(/* @__PURE__ */ f("keepThisCall"));
x(/* @__PURE__ */ g("keepThisCall"));

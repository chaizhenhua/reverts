// @__NO_SIDE_EFFECTS__
export default function f(y) {
  sideEffect(y);
}
onlyKeepThisIdentifier;
x(/* @__PURE__ */ f("keepThisCall"));

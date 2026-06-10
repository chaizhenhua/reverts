const f = /* @__NO_SIDE_EFFECTS__ */ function(y) {
  sideEffect(y);
}, g = /* @__NO_SIDE_EFFECTS__ */ function* (y) {
  sideEffect(y);
};
onlyKeepThisIdentifier;
onlyKeepThisIdentifier;
x(/* @__PURE__ */ f("keepThisCall"));
x(/* @__PURE__ */ g("keepThisCall"));

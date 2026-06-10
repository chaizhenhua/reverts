var TopLevelNumber = /* @__PURE__ */ ((TopLevelNumber2) => {
  TopLevelNumber2[TopLevelNumber2["foo_"] = 0] = "foo_";
  return TopLevelNumber2;
})(TopLevelNumber || {});
var TopLevelString = /* @__PURE__ */ ((TopLevelString2) => {
  TopLevelString2["bar_"] = "";
  return TopLevelString2;
})(TopLevelString || {});
console.log({
  foo: TopLevelNumber.a,
  bar: TopLevelString.b
});
function fn() {
  let NestedNumber;
  ((NestedNumber2) => {
    NestedNumber2[NestedNumber2["foo_"] = 0] = "foo_";
  })(NestedNumber || (NestedNumber = {}));
  let NestedString;
  ((NestedString2) => {
    NestedString2["bar_"] = "";
  })(NestedString || (NestedString = {}));
  console.log({
    foo: TopLevelNumber.a,
    bar: TopLevelString.b
  });
}

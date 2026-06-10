undef = void 0;
keepMe();
((x = keepMe()) => {
})();
var someVar;
(([y]) => {
})(someVar);
(({ z }) => {
})(someVar);
var keepThis = stuff();
keepThis();
((_ = keepMe()) => {
})();
var isPure = /* @__PURE__ */ ((x, y) => 123)();
use(isPure);
var isNotPure = ((x = foo, y = bar) => 123)();
use(isNotPure);
(async () => ({ get then() {
  notPure();
} }))();
(async function() {
  return { get then() {
    notPure();
  } };
})();

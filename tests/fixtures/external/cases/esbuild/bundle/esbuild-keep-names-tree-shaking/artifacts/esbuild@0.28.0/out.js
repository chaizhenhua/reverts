(() => {
  var __defProp = Object.defineProperty;
  var __name = (target, value) => __defProp(target, "name", { value, configurable: !0 });

  // entry.js
  function fnStmtKeep() {
  }
  __name(fnStmtKeep, "fnStmtKeep");
  x = fnStmtKeep;
  var fnExprKeep = /* @__PURE__ */ __name(function() {
  }, "keep");
  x = fnExprKeep;
  var clsStmtKeep = class {
    static {
      __name(this, "clsStmtKeep");
    }
  };
  new clsStmtKeep();
  var clsExprKeep = class {
    static {
      __name(this, "keep");
    }
  };
  new clsExprKeep();
})();

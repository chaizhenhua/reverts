var __knownSymbol = (name, symbol) => (symbol = Symbol[name]) ? symbol : /* @__PURE__ */ Symbol.for("Symbol." + name);
var __typeError = (msg) => {
  throw TypeError(msg);
};
var __async = (__this, __arguments, generator) => {
  return new Promise((resolve, reject) => {
    var fulfilled = (value) => {
      try {
        step(generator.next(value));
      } catch (e2) {
        reject(e2);
      }
    };
    var rejected = (value) => {
      try {
        step(generator.throw(value));
      } catch (e2) {
        reject(e2);
      }
    };
    var step = (x) => x.done ? resolve(x.value) : Promise.resolve(x.value).then(fulfilled, rejected);
    step((generator = generator.apply(__this, __arguments)).next());
  });
};
var __using = (stack, value, async) => {
  if (value != null) {
    if (typeof value !== "object" && typeof value !== "function") __typeError("Object expected");
    var dispose, inner;
    if (async) dispose = value[__knownSymbol("asyncDispose")];
    if (dispose === void 0) {
      dispose = value[__knownSymbol("dispose")];
      if (async) inner = dispose;
    }
    if (typeof dispose !== "function") __typeError("Object not disposable");
    if (inner) dispose = function() {
      try {
        inner.call(this);
      } catch (e2) {
        return Promise.reject(e2);
      }
    };
    stack.push([async, dispose, value]);
  } else if (async) {
    stack.push([async]);
  }
  return value;
};
var __callDispose = (stack, error, hasError) => {
  var E = typeof SuppressedError === "function" ? SuppressedError : function(e2, s, m, _) {
    return _ = Error(m), _.name = "SuppressedError", _.error = e2, _.suppressed = s, _;
  };
  var fail = (e2) => error = hasError ? new E(e2, error, "An error was suppressed during disposal") : (hasError = true, e2);
  var next = (it) => {
    while (it = stack.pop()) {
      try {
        var result = it[1] && it[1].call(it[2]);
        if (it[0]) return Promise.resolve(result).then(next, (e2) => (fail(e2), next()));
      } catch (e2) {
        fail(e2);
      }
    }
    if (hasError) throw error;
  };
  return next();
};
for (using a of b) c(() => a);
if (nested) {
  for (using a of b) c(() => a);
}
function foo() {
  for (using a of b) c(() => a);
}
function bar() {
  return __async(this, null, function* () {
    for (using a of b) c(() => a);
    for (var _d of e) {
      var _stack = [];
      try {
        const d = __using(_stack, _d, true);
        f(() => d);
      } catch (_) {
        var _error = _, _hasError = true;
      } finally {
        var _promise = __callDispose(_stack, _error, _hasError);
        _promise && (yield _promise);
      }
    }
  });
}

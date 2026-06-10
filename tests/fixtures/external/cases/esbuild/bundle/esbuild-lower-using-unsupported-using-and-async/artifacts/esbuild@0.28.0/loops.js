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
  var E = typeof SuppressedError === "function" ? SuppressedError : function(e2, s, m, _3) {
    return _3 = Error(m), _3.name = "SuppressedError", _3.error = e2, _3.suppressed = s, _3;
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
for (var _a of b) {
  var _stack = [];
  try {
    const a = __using(_stack, _a);
    c(() => a);
  } catch (_) {
    var _error = _, _hasError = true;
  } finally {
    __callDispose(_stack, _error, _hasError);
  }
}
if (nested) {
  for (var _a of b) {
    var _stack2 = [];
    try {
      const a = __using(_stack2, _a);
      c(() => a);
    } catch (_2) {
      var _error2 = _2, _hasError2 = true;
    } finally {
      __callDispose(_stack2, _error2, _hasError2);
    }
  }
}
function foo() {
  for (var _a of b) {
    var _stack3 = [];
    try {
      const a = __using(_stack3, _a);
      c(() => a);
    } catch (_3) {
      var _error3 = _3, _hasError3 = true;
    } finally {
      __callDispose(_stack3, _error3, _hasError3);
    }
  }
}
function bar() {
  return __async(this, null, function* () {
    for (var _a of b) {
      var _stack3 = [];
      try {
        const a = __using(_stack3, _a);
        c(() => a);
      } catch (_3) {
        var _error3 = _3, _hasError3 = true;
      } finally {
        __callDispose(_stack3, _error3, _hasError3);
      }
    }
    for (var _d of e) {
      var _stack4 = [];
      try {
        const d = __using(_stack4, _d, true);
        f(() => d);
      } catch (_4) {
        var _error4 = _4, _hasError4 = true;
      } finally {
        var _promise = __callDispose(_stack4, _error4, _hasError4);
        _promise && (yield _promise);
      }
    }
  });
}

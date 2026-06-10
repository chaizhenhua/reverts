var __knownSymbol = (name, symbol) => (symbol = Symbol[name]) ? symbol : /* @__PURE__ */ Symbol.for("Symbol." + name);
var __typeError = (msg) => {
  throw TypeError(msg);
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
      } catch (e) {
        return Promise.reject(e);
      }
    };
    stack.push([async, dispose, value]);
  } else if (async) {
    stack.push([async]);
  }
  return value;
};
var __callDispose = (stack, error, hasError) => {
  var E = typeof SuppressedError === "function" ? SuppressedError : function(e, s, m, _3) {
    return _3 = Error(m), _3.name = "SuppressedError", _3.error = e, _3.suppressed = s, _3;
  };
  var fail = (e) => error = hasError ? new E(e, error, "An error was suppressed during disposal") : (hasError = true, e);
  var next = (it) => {
    while (it = stack.pop()) {
      try {
        var result = it[1] && it[1].call(it[2]);
        if (it[0]) return Promise.resolve(result).then(next, (e) => (fail(e), next()));
      } catch (e) {
        fail(e);
      }
    }
    if (hasError) throw error;
  };
  return next();
};
function foo() {
  var _stack4 = [];
  try {
    const a2 = __using(_stack4, b);
    if (nested) {
      var _stack3 = [];
      try {
        const x = __using(_stack3, 1);
      } catch (_3) {
        var _error3 = _3, _hasError3 = true;
      } finally {
        __callDispose(_stack3, _error3, _hasError3);
      }
    }
  } catch (_4) {
    var _error4 = _4, _hasError4 = true;
  } finally {
    __callDispose(_stack4, _error4, _hasError4);
  }
}
async function bar() {
  var _stack4 = [];
  try {
    const a2 = __using(_stack4, b);
    const c2 = __using(_stack4, d, true);
    if (nested) {
      var _stack3 = [];
      try {
        const x = __using(_stack3, 1);
        const y = __using(_stack3, 2, true);
      } catch (_3) {
        var _error3 = _3, _hasError3 = true;
      } finally {
        var _promise3 = __callDispose(_stack3, _error3, _hasError3);
        _promise3 && await _promise3;
      }
    }
  } catch (_4) {
    var _error4 = _4, _hasError4 = true;
  } finally {
    var _promise4 = __callDispose(_stack4, _error4, _hasError4);
    _promise4 && await _promise4;
  }
}
var _stack2 = [];
try {
  var a = __using(_stack2, b);
  var c = __using(_stack2, d, true);
  if (nested) {
    var _stack = [];
    try {
      const x = __using(_stack, 1);
      const y = __using(_stack, 2, true);
    } catch (_) {
      var _error = _, _hasError = true;
    } finally {
      var _promise = __callDispose(_stack, _error, _hasError);
      _promise && await _promise;
    }
  }
} catch (_2) {
  var _error2 = _2, _hasError2 = true;
} finally {
  var _promise2 = __callDispose(_stack2, _error2, _hasError2);
  _promise2 && await _promise2;
}

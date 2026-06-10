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
  var E = typeof SuppressedError === "function" ? SuppressedError : function(e2, s, m, _9) {
    return _9 = Error(m), _9.name = "SuppressedError", _9.error = e2, _9.suppressed = s, _9;
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
for (var _d of e) {
  var _stack2 = [];
  try {
    const d = __using(_stack2, _d, true);
    f(() => d);
  } catch (_2) {
    var _error2 = _2, _hasError2 = true;
  } finally {
    var _promise = __callDispose(_stack2, _error2, _hasError2);
    _promise && await _promise;
  }
}
for await (var _g of h) {
  var _stack3 = [];
  try {
    const g = __using(_stack3, _g);
    i(() => g);
  } catch (_3) {
    var _error3 = _3, _hasError3 = true;
  } finally {
    __callDispose(_stack3, _error3, _hasError3);
  }
}
for await (var _j of k) {
  var _stack4 = [];
  try {
    const j = __using(_stack4, _j, true);
    l(() => j);
  } catch (_4) {
    var _error4 = _4, _hasError4 = true;
  } finally {
    var _promise2 = __callDispose(_stack4, _error4, _hasError4);
    _promise2 && await _promise2;
  }
}
if (nested) {
  for (var _a of b) {
    var _stack5 = [];
    try {
      const a = __using(_stack5, _a);
      c(() => a);
    } catch (_5) {
      var _error5 = _5, _hasError5 = true;
    } finally {
      __callDispose(_stack5, _error5, _hasError5);
    }
  }
  for (var _d of e) {
    var _stack6 = [];
    try {
      const d = __using(_stack6, _d, true);
      f(() => d);
    } catch (_6) {
      var _error6 = _6, _hasError6 = true;
    } finally {
      var _promise3 = __callDispose(_stack6, _error6, _hasError6);
      _promise3 && await _promise3;
    }
  }
  for await (var _g of h) {
    var _stack7 = [];
    try {
      const g = __using(_stack7, _g);
      i(() => g);
    } catch (_7) {
      var _error7 = _7, _hasError7 = true;
    } finally {
      __callDispose(_stack7, _error7, _hasError7);
    }
  }
  for await (var _j of k) {
    var _stack8 = [];
    try {
      const j = __using(_stack8, _j, true);
      l(() => j);
    } catch (_8) {
      var _error8 = _8, _hasError8 = true;
    } finally {
      var _promise4 = __callDispose(_stack8, _error8, _hasError8);
      _promise4 && await _promise4;
    }
  }
}
function foo() {
  for (var _a of b) {
    var _stack9 = [];
    try {
      const a = __using(_stack9, _a);
      c(() => a);
    } catch (_9) {
      var _error9 = _9, _hasError9 = true;
    } finally {
      __callDispose(_stack9, _error9, _hasError9);
    }
  }
}
async function bar() {
  for (var _a of b) {
    var _stack9 = [];
    try {
      const a = __using(_stack9, _a);
      c(() => a);
    } catch (_9) {
      var _error9 = _9, _hasError9 = true;
    } finally {
      __callDispose(_stack9, _error9, _hasError9);
    }
  }
  for (var _d of e) {
    var _stack10 = [];
    try {
      const d = __using(_stack10, _d, true);
      f(() => d);
    } catch (_10) {
      var _error10 = _10, _hasError10 = true;
    } finally {
      var _promise5 = __callDispose(_stack10, _error10, _hasError10);
      _promise5 && await _promise5;
    }
  }
  for await (var _g of h) {
    var _stack11 = [];
    try {
      const g = __using(_stack11, _g);
      i(() => g);
    } catch (_11) {
      var _error11 = _11, _hasError11 = true;
    } finally {
      __callDispose(_stack11, _error11, _hasError11);
    }
  }
  for await (var _j of k) {
    var _stack12 = [];
    try {
      const j = __using(_stack12, _j, true);
      l(() => j);
    } catch (_12) {
      var _error12 = _12, _hasError12 = true;
    } finally {
      var _promise6 = __callDispose(_stack12, _error12, _hasError12);
      _promise6 && await _promise6;
    }
  }
}

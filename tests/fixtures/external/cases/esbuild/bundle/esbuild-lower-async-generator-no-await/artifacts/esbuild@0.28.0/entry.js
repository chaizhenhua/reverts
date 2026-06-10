var __knownSymbol = (name, symbol) => (symbol = Symbol[name]) ? symbol : /* @__PURE__ */ Symbol.for("Symbol." + name);
var __typeError = (msg) => {
  throw TypeError(msg);
};
var __async = (__this, __arguments, generator) => {
  return new Promise((resolve, reject) => {
    var fulfilled = (value) => {
      try {
        step(generator.next(value));
      } catch (e) {
        reject(e);
      }
    };
    var rejected = (value) => {
      try {
        step(generator.throw(value));
      } catch (e) {
        reject(e);
      }
    };
    var step = (x) => x.done ? resolve(x.value) : Promise.resolve(x.value).then(fulfilled, rejected);
    step((generator = generator.apply(__this, __arguments)).next());
  });
};
var __await = function(promise, isYieldStar) {
  this[0] = promise;
  this[1] = isYieldStar;
};
var __asyncGenerator = (__this, __arguments, generator) => {
  var resume = (k, v, yes, no) => {
    try {
      var x = generator[k](v), isAwait = (v = x.value) instanceof __await, done = x.done;
      Promise.resolve(isAwait ? v[0] : v).then((y2) => isAwait ? resume(k === "return" ? k : "next", v[1] ? { done: y2.done, value: y2.value } : y2, yes, no) : yes({ value: y2, done })).catch((e) => resume("throw", e, yes, no));
    } catch (e) {
      no(e);
    }
  }, method = (k, call, wait, clear) => it[k] = (x) => (call = new Promise((yes, no, run) => (run = () => resume(k, x, yes, no), q ? q.then(run) : run())), clear = () => q === wait && (q = 0), q = wait = call.then(clear, clear), call), q, it = {};
  return generator = generator.apply(__this, __arguments), it[__knownSymbol("asyncIterator")] = () => it, method("next"), method("throw"), method("return"), it;
};
var __yieldStar = (value) => {
  var obj = value[__knownSymbol("asyncIterator")], isAwait = false, method, it = {};
  if (obj == null) {
    obj = value[__knownSymbol("iterator")]();
    method = (k) => it[k] = (x) => obj[k](x);
  } else {
    obj = obj.call(value);
    method = (k) => it[k] = (v) => {
      if (isAwait) {
        isAwait = false;
        if (k === "throw") throw v;
        return v;
      }
      isAwait = true;
      return {
        done: false,
        value: new __await(new Promise((resolve) => {
          var x = obj[k](v);
          if (!(x instanceof Object)) __typeError("Object expected");
          resolve(x);
        }), 1)
      };
    };
  }
  return it[__knownSymbol("iterator")] = () => it, method("next"), "throw" in obj ? method("throw") : it.throw = (x) => {
    throw x;
  }, "return" in obj && method("return"), it;
};
var __forAwait = (obj, it, method) => (it = obj[__knownSymbol("asyncIterator")]) ? it.call(obj) : (obj = obj[__knownSymbol("iterator")](), it = {}, method = (key, fn) => (fn = obj[key]) && (it[key] = (arg) => new Promise((yes, no, done) => (arg = fn.call(obj, arg), done = arg.done, Promise.resolve(arg.value).then((value) => yes({ value, done }), no)))), method("next"), method("return"), it);
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
  var E = typeof SuppressedError === "function" ? SuppressedError : function(e, s, m, _) {
    return _ = Error(m), _.name = "SuppressedError", _.error = e, _.suppressed = s, _;
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
  return __asyncGenerator(this, null, function* () {
    var _stack2 = [];
    try {
      yield;
      yield x;
      yield* __yieldStar(x);
      const x = __using(_stack2, yield new __await(y), true);
      try {
        for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield new __await(iter.next())).done; more = false) {
          let x2 = temp.value;
        }
      } catch (temp) {
        error = [temp];
      } finally {
        try {
          more && (temp = iter.return) && (yield new __await(temp.call(iter)));
        } finally {
          if (error)
            throw error[0];
        }
      }
      try {
        for (var iter2 = __forAwait(y), more2, temp2, error2; more2 = !(temp2 = yield new __await(iter2.next())).done; more2 = false) {
          var _x = temp2.value;
          var _stack = [];
          try {
            const x2 = __using(_stack, _x, true);
          } catch (_) {
            var _error = _, _hasError = true;
          } finally {
            var _promise = __callDispose(_stack, _error, _hasError);
            _promise && (yield new __await(_promise));
          }
        }
      } catch (temp2) {
        error2 = [temp2];
      } finally {
        try {
          more2 && (temp2 = iter2.return) && (yield new __await(temp2.call(iter2)));
        } finally {
          if (error2)
            throw error2[0];
        }
      }
    } catch (_2) {
      var _error2 = _2, _hasError2 = true;
    } finally {
      var _promise2 = __callDispose(_stack2, _error2, _hasError2);
      _promise2 && (yield new __await(_promise2));
    }
  });
}
foo = function() {
  return __asyncGenerator(this, null, function* () {
    var _stack2 = [];
    try {
      yield;
      yield x;
      yield* __yieldStar(x);
      const x = __using(_stack2, yield new __await(y), true);
      try {
        for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield new __await(iter.next())).done; more = false) {
          let x2 = temp.value;
        }
      } catch (temp) {
        error = [temp];
      } finally {
        try {
          more && (temp = iter.return) && (yield new __await(temp.call(iter)));
        } finally {
          if (error)
            throw error[0];
        }
      }
      try {
        for (var iter2 = __forAwait(y), more2, temp2, error2; more2 = !(temp2 = yield new __await(iter2.next())).done; more2 = false) {
          var _x = temp2.value;
          var _stack = [];
          try {
            const x2 = __using(_stack, _x, true);
          } catch (_) {
            var _error = _, _hasError = true;
          } finally {
            var _promise = __callDispose(_stack, _error, _hasError);
            _promise && (yield new __await(_promise));
          }
        }
      } catch (temp2) {
        error2 = [temp2];
      } finally {
        try {
          more2 && (temp2 = iter2.return) && (yield new __await(temp2.call(iter2)));
        } finally {
          if (error2)
            throw error2[0];
        }
      }
    } catch (_2) {
      var _error2 = _2, _hasError2 = true;
    } finally {
      var _promise2 = __callDispose(_stack2, _error2, _hasError2);
      _promise2 && (yield new __await(_promise2));
    }
  });
};
foo = { bar() {
  return __asyncGenerator(this, null, function* () {
    var _stack2 = [];
    try {
      yield;
      yield x;
      yield* __yieldStar(x);
      const x = __using(_stack2, yield new __await(y), true);
      try {
        for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield new __await(iter.next())).done; more = false) {
          let x2 = temp.value;
        }
      } catch (temp) {
        error = [temp];
      } finally {
        try {
          more && (temp = iter.return) && (yield new __await(temp.call(iter)));
        } finally {
          if (error)
            throw error[0];
        }
      }
      try {
        for (var iter2 = __forAwait(y), more2, temp2, error2; more2 = !(temp2 = yield new __await(iter2.next())).done; more2 = false) {
          var _x = temp2.value;
          var _stack = [];
          try {
            const x2 = __using(_stack, _x, true);
          } catch (_) {
            var _error = _, _hasError = true;
          } finally {
            var _promise = __callDispose(_stack, _error, _hasError);
            _promise && (yield new __await(_promise));
          }
        }
      } catch (temp2) {
        error2 = [temp2];
      } finally {
        try {
          more2 && (temp2 = iter2.return) && (yield new __await(temp2.call(iter2)));
        } finally {
          if (error2)
            throw error2[0];
        }
      }
    } catch (_2) {
      var _error2 = _2, _hasError2 = true;
    } finally {
      var _promise2 = __callDispose(_stack2, _error2, _hasError2);
      _promise2 && (yield new __await(_promise2));
    }
  });
} };
class Foo {
  bar() {
    return __asyncGenerator(this, null, function* () {
      var _stack2 = [];
      try {
        yield;
        yield x;
        yield* __yieldStar(x);
        const x = __using(_stack2, yield new __await(y), true);
        try {
          for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield new __await(iter.next())).done; more = false) {
            let x2 = temp.value;
          }
        } catch (temp) {
          error = [temp];
        } finally {
          try {
            more && (temp = iter.return) && (yield new __await(temp.call(iter)));
          } finally {
            if (error)
              throw error[0];
          }
        }
        try {
          for (var iter2 = __forAwait(y), more2, temp2, error2; more2 = !(temp2 = yield new __await(iter2.next())).done; more2 = false) {
            var _x = temp2.value;
            var _stack = [];
            try {
              const x2 = __using(_stack, _x, true);
            } catch (_) {
              var _error = _, _hasError = true;
            } finally {
              var _promise = __callDispose(_stack, _error, _hasError);
              _promise && (yield new __await(_promise));
            }
          }
        } catch (temp2) {
          error2 = [temp2];
        } finally {
          try {
            more2 && (temp2 = iter2.return) && (yield new __await(temp2.call(iter2)));
          } finally {
            if (error2)
              throw error2[0];
          }
        }
      } catch (_2) {
        var _error2 = _2, _hasError2 = true;
      } finally {
        var _promise2 = __callDispose(_stack2, _error2, _hasError2);
        _promise2 && (yield new __await(_promise2));
      }
    });
  }
}
Foo = class {
  bar() {
    return __asyncGenerator(this, null, function* () {
      var _stack2 = [];
      try {
        yield;
        yield x;
        yield* __yieldStar(x);
        const x = __using(_stack2, yield new __await(y), true);
        try {
          for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield new __await(iter.next())).done; more = false) {
            let x2 = temp.value;
          }
        } catch (temp) {
          error = [temp];
        } finally {
          try {
            more && (temp = iter.return) && (yield new __await(temp.call(iter)));
          } finally {
            if (error)
              throw error[0];
          }
        }
        try {
          for (var iter2 = __forAwait(y), more2, temp2, error2; more2 = !(temp2 = yield new __await(iter2.next())).done; more2 = false) {
            var _x = temp2.value;
            var _stack = [];
            try {
              const x2 = __using(_stack, _x, true);
            } catch (_) {
              var _error = _, _hasError = true;
            } finally {
              var _promise = __callDispose(_stack, _error, _hasError);
              _promise && (yield new __await(_promise));
            }
          }
        } catch (temp2) {
          error2 = [temp2];
        } finally {
          try {
            more2 && (temp2 = iter2.return) && (yield new __await(temp2.call(iter2)));
          } finally {
            if (error2)
              throw error2[0];
          }
        }
      } catch (_2) {
        var _error2 = _2, _hasError2 = true;
      } finally {
        var _promise2 = __callDispose(_stack2, _error2, _hasError2);
        _promise2 && (yield new __await(_promise2));
      }
    });
  }
};
function bar() {
  return __async(this, null, function* () {
    var _stack2 = [];
    try {
      const x = __using(_stack2, yield y, true);
      try {
        for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield iter.next()).done; more = false) {
          let x2 = temp.value;
        }
      } catch (temp) {
        error = [temp];
      } finally {
        try {
          more && (temp = iter.return) && (yield temp.call(iter));
        } finally {
          if (error)
            throw error[0];
        }
      }
      try {
        for (var iter2 = __forAwait(y), more2, temp2, error2; more2 = !(temp2 = yield iter2.next()).done; more2 = false) {
          var _x = temp2.value;
          var _stack = [];
          try {
            const x2 = __using(_stack, _x, true);
          } catch (_) {
            var _error = _, _hasError = true;
          } finally {
            var _promise = __callDispose(_stack, _error, _hasError);
            _promise && (yield _promise);
          }
        }
      } catch (temp2) {
        error2 = [temp2];
      } finally {
        try {
          more2 && (temp2 = iter2.return) && (yield temp2.call(iter2));
        } finally {
          if (error2)
            throw error2[0];
        }
      }
    } catch (_2) {
      var _error2 = _2, _hasError2 = true;
    } finally {
      var _promise2 = __callDispose(_stack2, _error2, _hasError2);
      _promise2 && (yield _promise2);
    }
  });
}

var __knownSymbol = (name, symbol) => (symbol = Symbol[name]) ? symbol : /* @__PURE__ */ Symbol.for("Symbol." + name);
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
    var step = (x2) => x2.done ? resolve(x2.value) : Promise.resolve(x2.value).then(fulfilled, rejected);
    step((generator = generator.apply(__this, __arguments)).next());
  });
};
var __forAwait = (obj, it, method) => (it = obj[__knownSymbol("asyncIterator")]) ? it.call(obj) : (obj = obj[__knownSymbol("iterator")](), it = {}, method = (key, fn) => (fn = obj[key]) && (it[key] = (arg) => new Promise((yes, no, done) => (arg = fn.call(obj, arg), done = arg.done, Promise.resolve(arg.value).then((value) => yes({ value, done }), no)))), method("next"), method("return"), it);
export default [
  () => __async(null, null, function* () {
    try {
      for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield iter.next()).done; more = false) {
        x = temp.value;
        z(x);
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
  }),
  () => __async(null, null, function* () {
    try {
      for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield iter.next()).done; more = false) {
        x.y = temp.value;
        z(x);
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
  }),
  () => __async(null, null, function* () {
    try {
      for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield iter.next()).done; more = false) {
        let x2 = temp.value;
        z(x2);
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
  }),
  () => __async(null, null, function* () {
    try {
      for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield iter.next()).done; more = false) {
        const x2 = temp.value;
        z(x2);
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
  }),
  () => __async(null, null, function* () {
    try {
      label: for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield iter.next()).done; more = false) {
        const x2 = temp.value;
        break label;
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
  }),
  () => __async(null, null, function* () {
    try {
      label: for (var iter = __forAwait(y), more, temp, error; more = !(temp = yield iter.next()).done; more = false) {
        const x2 = temp.value;
        continue label;
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
  })
];

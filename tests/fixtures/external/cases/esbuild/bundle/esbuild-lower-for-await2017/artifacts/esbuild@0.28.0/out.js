var __knownSymbol = (name, symbol) => (symbol = Symbol[name]) ? symbol : /* @__PURE__ */ Symbol.for("Symbol." + name);
var __forAwait = (obj, it, method) => (it = obj[__knownSymbol("asyncIterator")]) ? it.call(obj) : (obj = obj[__knownSymbol("iterator")](), it = {}, method = (key, fn) => (fn = obj[key]) && (it[key] = (arg) => new Promise((yes, no, done) => (arg = fn.call(obj, arg), done = arg.done, Promise.resolve(arg.value).then((value) => yes({ value, done }), no)))), method("next"), method("return"), it);
export default [
  async () => {
    try {
      for (var iter = __forAwait(y), more, temp, error; more = !(temp = await iter.next()).done; more = false) {
        x = temp.value;
        z(x);
      }
    } catch (temp) {
      error = [temp];
    } finally {
      try {
        more && (temp = iter.return) && await temp.call(iter);
      } finally {
        if (error)
          throw error[0];
      }
    }
  },
  async () => {
    try {
      for (var iter = __forAwait(y), more, temp, error; more = !(temp = await iter.next()).done; more = false) {
        x.y = temp.value;
        z(x);
      }
    } catch (temp) {
      error = [temp];
    } finally {
      try {
        more && (temp = iter.return) && await temp.call(iter);
      } finally {
        if (error)
          throw error[0];
      }
    }
  },
  async () => {
    try {
      for (var iter = __forAwait(y), more, temp, error; more = !(temp = await iter.next()).done; more = false) {
        let x2 = temp.value;
        z(x2);
      }
    } catch (temp) {
      error = [temp];
    } finally {
      try {
        more && (temp = iter.return) && await temp.call(iter);
      } finally {
        if (error)
          throw error[0];
      }
    }
  },
  async () => {
    try {
      for (var iter = __forAwait(y), more, temp, error; more = !(temp = await iter.next()).done; more = false) {
        const x2 = temp.value;
        z(x2);
      }
    } catch (temp) {
      error = [temp];
    } finally {
      try {
        more && (temp = iter.return) && await temp.call(iter);
      } finally {
        if (error)
          throw error[0];
      }
    }
  },
  async () => {
    try {
      label: for (var iter = __forAwait(y), more, temp, error; more = !(temp = await iter.next()).done; more = false) {
        const x2 = temp.value;
        break label;
      }
    } catch (temp) {
      error = [temp];
    } finally {
      try {
        more && (temp = iter.return) && await temp.call(iter);
      } finally {
        if (error)
          throw error[0];
      }
    }
  },
  async () => {
    try {
      label: for (var iter = __forAwait(y), more, temp, error; more = !(temp = await iter.next()).done; more = false) {
        const x2 = temp.value;
        continue label;
      }
    } catch (temp) {
      error = [temp];
    } finally {
      try {
        more && (temp = iter.return) && await temp.call(iter);
      } finally {
        if (error)
          throw error[0];
      }
    }
  }
];

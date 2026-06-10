(() => {
  var __create = Object.create;
  var __defProp = Object.defineProperty;
  var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
  var __knownSymbol = (name, symbol) => (symbol = Symbol[name]) ? symbol : /* @__PURE__ */ Symbol.for("Symbol." + name);
  var __typeError = (msg) => {
    throw TypeError(msg);
  };
  var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
  var __name = (target, value) => __defProp(target, "name", { value, configurable: true });
  var __decoratorStart = (base) => [, , , __create(base?.[__knownSymbol("metadata")] ?? null)];
  var __decoratorStrings = ["class", "method", "getter", "setter", "accessor", "field", "value", "get", "set"];
  var __expectFn = (fn) => fn !== void 0 && typeof fn !== "function" ? __typeError("Function expected") : fn;
  var __decoratorContext = (kind, name, done, metadata, fns) => ({ kind: __decoratorStrings[kind], name, metadata, addInitializer: (fn) => done._ ? __typeError("Already initialized") : fns.push(__expectFn(fn || null)) });
  var __decoratorMetadata = (array, target) => __defNormalProp(target, __knownSymbol("metadata"), array[3]);
  var __runInitializers = (array, flags, self, value) => {
    for (var i = 0, fns = array[flags >> 1], n = fns && fns.length; i < n; i++) flags & 1 ? fns[i].call(self) : value = fns[i].call(self, value);
    return value;
  };
  var __decorateElement = (array, flags, name, decorators, target, extra) => {
    var fn, it, done, ctx, access, k = flags & 7, s = !!(flags & 8), p = !!(flags & 16);
    var j = k > 3 ? array.length + 1 : k ? s ? 1 : 2 : 0, key = __decoratorStrings[k + 5];
    var initializers = k > 3 && (array[j - 1] = []), extraInitializers = array[j] || (array[j] = []);
    var desc = k && (!p && !s && (target = target.prototype), k < 5 && (k > 3 || !p) && __getOwnPropDesc(k < 4 ? target : { get [name]() {
      return __privateGet(this, extra);
    }, set [name](x) {
      return __privateSet(this, extra, x);
    } }, name));
    k ? p && k < 4 && __name(extra, (k > 2 ? "set " : k > 1 ? "get " : "") + name) : __name(target, name);
    for (var i = decorators.length - 1; i >= 0; i--) {
      ctx = __decoratorContext(k, name, done = {}, array[3], extraInitializers);
      if (k) {
        ctx.static = s, ctx.private = p, access = ctx.access = { has: p ? (x) => __privateIn(target, x) : (x) => name in x };
        if (k ^ 3) access.get = p ? (x) => (k ^ 1 ? __privateGet : __privateMethod)(x, target, k ^ 4 ? extra : desc.get) : (x) => x[name];
        if (k > 2) access.set = p ? (x, y) => __privateSet(x, target, y, k ^ 4 ? extra : desc.set) : (x, y) => x[name] = y;
      }
      it = (0, decorators[i])(k ? k < 4 ? p ? extra : desc[key] : k > 4 ? void 0 : { get: desc.get, set: desc.set } : target, ctx), done._ = 1;
      if (k ^ 4 || it === void 0) __expectFn(it) && (k > 4 ? initializers.unshift(it) : k ? p ? extra = it : desc[key] = it : target = it);
      else if (typeof it !== "object" || it === null) __typeError("Object expected");
      else __expectFn(fn = it.get) && (desc.get = fn), __expectFn(fn = it.set) && (desc.set = fn), __expectFn(fn = it.init) && initializers.unshift(fn);
    }
    return k || __decoratorMetadata(array, target), desc && __defProp(target, name, desc), p ? k ^ 4 ? extra : desc : target;
  };
  var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
  var __privateIn = (member, obj) => Object(obj) !== obj ? __typeError('Cannot use the "in" operator on this value') : member.has(obj);
  var __privateGet = (obj, member, getter) => (__accessCheck(obj, member, "read from private field"), getter ? getter.call(obj) : member.get(obj));
  var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
  var __privateSet = (obj, member, value, setter) => (__accessCheck(obj, member, "write to private field"), setter ? setter.call(obj, value) : member.set(obj, value), value);
  var __privateMethod = (obj, member, method) => (__accessCheck(obj, member, "access private method"), method);

  // input/Users/user/project/src/entrywithdec.ts
  var _Class_decorators, _init;
  _Class_decorators = [dec];
  var Class = class {
  };
  _init = __decoratorStart(null);
  Class = __decorateElement(_init, 0, "Class", _Class_decorators, Class);
  __runInitializers(_init, 1, Class);
  var _foo_dec, _init2;
  _foo_dec = [dec];
  var ClassMethod = class {
    constructor() {
      __runInitializers(_init2, 5, this);
    }
    foo() {
    }
  };
  _init2 = __decoratorStart(null);
  __decorateElement(_init2, 1, "foo", _foo_dec, ClassMethod);
  __decoratorMetadata(_init2, ClassMethod);
  var _bar_dec, _foo_dec2, _init3;
  _foo_dec2 = [dec], _bar_dec = [dec];
  var ClassField = class {
    constructor() {
      this.foo = __runInitializers(_init3, 8, this, 123), __runInitializers(_init3, 11, this);
      this.bar = __runInitializers(_init3, 12, this), __runInitializers(_init3, 15, this);
    }
  };
  _init3 = __decoratorStart(null);
  __decorateElement(_init3, 5, "foo", _foo_dec2, ClassField);
  __decorateElement(_init3, 5, "bar", _bar_dec, ClassField);
  __decoratorMetadata(_init3, ClassField);
  var _bar_dec2, _foo_dec3, _init4, _foo, _bar;
  _foo_dec3 = [dec], _bar_dec2 = [dec];
  var ClassAccessor = class {
    constructor() {
      __privateAdd(this, _foo, __runInitializers(_init4, 8, this, 123)), __runInitializers(_init4, 11, this);
      __privateAdd(this, _bar, __runInitializers(_init4, 12, this)), __runInitializers(_init4, 15, this);
    }
  };
  _init4 = __decoratorStart(null);
  _foo = new WeakMap();
  _bar = new WeakMap();
  __decorateElement(_init4, 4, "foo", _foo_dec3, ClassAccessor, _foo);
  __decorateElement(_init4, 4, "bar", _bar_dec2, ClassAccessor, _bar);
  __decoratorMetadata(_init4, ClassAccessor);
  new Class();
  new ClassMethod();
  new ClassField();
  new ClassAccessor();
})();

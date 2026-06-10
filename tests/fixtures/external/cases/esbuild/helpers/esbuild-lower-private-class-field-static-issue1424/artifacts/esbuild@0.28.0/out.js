(() => {
  var __defProp = Object.defineProperty;
  var __typeError = (msg) => {
    throw TypeError(msg);
  };
  var __defNormalProp = (obj, key, value) => key in obj ? __defProp(obj, key, { enumerable: true, configurable: true, writable: true, value }) : obj[key] = value;
  var __publicField = (obj, key, value) => __defNormalProp(obj, typeof key !== "symbol" ? key + "" : key, value);
  var __accessCheck = (obj, member, msg) => member.has(obj) || __typeError("Cannot " + msg);
  var __privateAdd = (obj, member, value) => member.has(obj) ? __typeError("Cannot add the same private member more than once") : member instanceof WeakSet ? member.add(obj) : member.set(obj, value);
  var __privateMethod = (obj, member, method) => (__accessCheck(obj, member, "access private method"), method);

  // input/entry.js
  var _T_instances, a_fn, b_fn;
  var T = class {
    constructor() {
      __privateAdd(this, _T_instances);
    }
    d() {
      console.log(__privateMethod(this, _T_instances, a_fn).call(this));
    }
  };
  _T_instances = new WeakSet();
  a_fn = function() {
    return "a";
  };
  b_fn = function() {
    return "b";
  };
  __publicField(T, "c");
  new T().d();
})();

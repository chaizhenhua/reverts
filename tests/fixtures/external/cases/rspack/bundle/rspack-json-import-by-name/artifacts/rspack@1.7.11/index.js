(() => {
"use strict";
var __webpack_modules__ = ({
"./data/c.json"
/*!*********************!*\
  !*** ./data/c.json ***!
  \*********************/
(module) {
module.exports = [1,2,3,4]

},
"./data/d.json"
/*!*********************!*\
  !*** ./data/d.json ***!
  \*********************/
(module) {
module.exports = {}

},
"./data/e.json"
/*!*********************!*\
  !*** ./data/e.json ***!
  \*********************/
(module) {
module.exports = JSON.parse('{"1":"x","bb":2,"aa":1}')

},
"./data/f.json"
/*!*********************!*\
  !*** ./data/f.json ***!
  \*********************/
(module) {
module.exports = JSON.parse('{"named":"named","default":"default","__esModule":true}')

},
"./data/g.json"
/*!*********************!*\
  !*** ./data/g.json ***!
  \*********************/
(module) {
module.exports = {"named":{}}

},

});
// The module cache
var __webpack_module_cache__ = {};

// The require function
function __webpack_require__(moduleId) {

// Check if module is in cache
var cachedModule = __webpack_module_cache__[moduleId];
if (cachedModule !== undefined) {
return cachedModule.exports;
}
// Create a new module (and put it into the cache)
var module = (__webpack_module_cache__[moduleId] = {
exports: {}
});
// Execute the module function
__webpack_modules__[moduleId](module, module.exports, __webpack_require__);

// Return the exports of the module
return module.exports;

}

// webpack/runtime/create_fake_namespace_object
(() => {
var getProto = Object.getPrototypeOf ? (obj) => (Object.getPrototypeOf(obj)) : (obj) => (obj.__proto__);
var leafPrototypes;
// create a fake namespace object
// mode & 1: value is a module id, require it
// mode & 2: merge all properties of value into the ns
// mode & 4: return value when already ns object
// mode & 16: return value when it's Promise-like
// mode & 8|1: behave like require
__webpack_require__.t = function(value, mode) {
	if(mode & 1) value = this(value);
	if(mode & 8) return value;
	if(typeof value === 'object' && value) {
		if((mode & 4) && value.__esModule) return value;
		if((mode & 16) && typeof value.then === 'function') return value;
	}
	var ns = Object.create(null);
  __webpack_require__.r(ns);
	var def = {};
	leafPrototypes = leafPrototypes || [null, getProto({}), getProto([]), getProto(getProto)];
	for(var current = mode & 2 && value; (typeof current == 'object' || typeof current == 'function') && !~leafPrototypes.indexOf(current); current = getProto(current)) {
		Object.getOwnPropertyNames(current).forEach((key) => { def[key] = () => (value[key]) });
	}
	def['default'] = () => (value);
	__webpack_require__.d(ns, def);
	return ns;
};
})();
// webpack/runtime/define_property_getters
(() => {
__webpack_require__.d = (exports, definition) => {
	for(var key in definition) {
        if(__webpack_require__.o(definition, key) && !__webpack_require__.o(exports, key)) {
            Object.defineProperty(exports, key, { enumerable: true, get: definition[key] });
        }
    }
};
})();
// webpack/runtime/has_own_property
(() => {
__webpack_require__.o = (obj, prop) => (Object.prototype.hasOwnProperty.call(obj, prop))
})();
// webpack/runtime/make_namespace_object
(() => {
// define __esModule on exports
__webpack_require__.r = (exports) => {
	if(typeof Symbol !== 'undefined' && Symbol.toStringTag) {
		Object.defineProperty(exports, Symbol.toStringTag, { value: 'Module' });
	}
	Object.defineProperty(exports, '__esModule', { value: true });
};
})();
// webpack/runtime/rspack_version
(() => {
__webpack_require__.rv = () => ("1.7.11")
})();
// webpack/runtime/rspack_unique_id
(() => {
__webpack_require__.ruid = "bundler=rspack@1.7.11";
})();
var __webpack_exports__ = {};
// This entry needs to be wrapped in an IIFE because it needs to be isolated against other modules in the chunk.
(() => {

/*!******************!*\
  !*** ./index.js ***!
  \******************/
var _data_c_json__rspack_import_0_namespace_cache;
var _data_d_json__rspack_import_1_namespace_cache;
__webpack_require__.r(__webpack_exports__);
/* import */ var _data_c_json__rspack_import_0 = __webpack_require__(/*! ./data/c.json */ "./data/c.json");
/* import */ var _data_d_json__rspack_import_1 = __webpack_require__(/*! ./data/d.json */ "./data/d.json");
/* import */ var _data_e_json__rspack_import_2 = __webpack_require__(/*! ./data/e.json */ "./data/e.json");
/* import */ var _data_f_json__rspack_import_3 = __webpack_require__(/*! ./data/f.json */ "./data/f.json");
/* import */ var _data_g_json__rspack_import_4 = __webpack_require__(/*! ./data/g.json */ "./data/g.json");





console.log(/*#__PURE__*/ (_data_c_json__rspack_import_0_namespace_cache || (_data_c_json__rspack_import_0_namespace_cache = __webpack_require__.t(_data_c_json__rspack_import_0, 2))), /*#__PURE__*/ (_data_d_json__rspack_import_1_namespace_cache || (_data_d_json__rspack_import_1_namespace_cache = __webpack_require__.t(_data_d_json__rspack_import_1, 2))), _data_e_json__rspack_import_2.aa, _data_e_json__rspack_import_2.bb, _data_f_json__rspack_import_3, _data_f_json__rspack_import_3.named, _data_g_json__rspack_import_4, _data_g_json__rspack_import_4.named);

})();

})()
;
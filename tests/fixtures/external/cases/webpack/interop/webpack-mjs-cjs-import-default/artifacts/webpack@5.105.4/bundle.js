/******/ (() => { // webpackBootstrap
/******/ 	var __webpack_modules__ = ([
/* 0 */
/*!****************!*\
  !*** ./cjs.js ***!
  \****************/
/***/ ((module) => {

module.exports = {
	data: "ok",
	default: "default"
};

/***/ }),
/* 1 */
/*!********************!*\
  !*** ./flagged.js ***!
  \********************/
/***/ ((__unused_webpack_module, exports) => {

exports.__esModule = true;
exports.data = "ok";
exports["default"] = "default";

/***/ }),
/* 2 */
/*!********************!*\
  !*** ./dynamic.js ***!
  \********************/
/***/ ((__unused_webpack_module, exports) => {

exports.__esModule = Math.random() < -1;
exports.data = "ok";
exports["default"] = "default";

/***/ }),
/* 3 */
/*!***************************!*\
  !*** ./dynamicFlagged.js ***!
  \***************************/
/***/ ((__unused_webpack_module, exports) => {

exports.__esModule = Math.random() > -1;
exports.data = "ok";
exports["default"] = "default";

/***/ }),
/* 4 */
/*!**********************!*\
  !*** ./reexport.mjs ***!
  \**********************/
/***/ ((__unused_webpack___webpack_module__, __webpack_exports__, __webpack_require__) => {

"use strict";
var _cjs_js__WEBPACK_IMPORTED_MODULE_0___namespace_cache;
__webpack_require__.r(__webpack_exports__);
/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   data: () => (/* reexport safe */ _cjs_js__WEBPACK_IMPORTED_MODULE_0__.data),
/* harmony export */   def: () => (/* reexport default export from named module */ _cjs_js__WEBPACK_IMPORTED_MODULE_0__),
/* harmony export */   "default": () => (/* reexport default export from named module */ _cjs_js__WEBPACK_IMPORTED_MODULE_0__),
/* harmony export */   ns: () => (/* reexport fake namespace object from non-harmony */ _cjs_js__WEBPACK_IMPORTED_MODULE_0___namespace_cache || (_cjs_js__WEBPACK_IMPORTED_MODULE_0___namespace_cache = __webpack_require__.t(_cjs_js__WEBPACK_IMPORTED_MODULE_0__, 2)))
/* harmony export */ });
/* harmony import */ var _cjs_js__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./cjs.js */ 0);






/***/ })
/******/ 	]);
/************************************************************************/
/******/ 	// The module cache
/******/ 	var __webpack_module_cache__ = {};
/******/ 	
/******/ 	// The require function
/******/ 	function __webpack_require__(moduleId) {
/******/ 		// Check if module is in cache
/******/ 		var cachedModule = __webpack_module_cache__[moduleId];
/******/ 		if (cachedModule !== undefined) {
/******/ 			return cachedModule.exports;
/******/ 		}
/******/ 		// Create a new module (and put it into the cache)
/******/ 		var module = __webpack_module_cache__[moduleId] = {
/******/ 			// no module.id needed
/******/ 			// no module.loaded needed
/******/ 			exports: {}
/******/ 		};
/******/ 	
/******/ 		// Execute the module function
/******/ 		if (!(moduleId in __webpack_modules__)) {
/******/ 			delete __webpack_module_cache__[moduleId];
/******/ 			var e = new Error("Cannot find module '" + moduleId + "'");
/******/ 			e.code = 'MODULE_NOT_FOUND';
/******/ 			throw e;
/******/ 		}
/******/ 		__webpack_modules__[moduleId](module, module.exports, __webpack_require__);
/******/ 	
/******/ 		// Return the exports of the module
/******/ 		return module.exports;
/******/ 	}
/******/ 	
/************************************************************************/
/******/ 	/* webpack/runtime/create fake namespace object */
/******/ 	(() => {
/******/ 		var getProto = Object.getPrototypeOf ? (obj) => (Object.getPrototypeOf(obj)) : (obj) => (obj.__proto__);
/******/ 		var leafPrototypes;
/******/ 		// create a fake namespace object
/******/ 		// mode & 1: value is a module id, require it
/******/ 		// mode & 2: merge all properties of value into the ns
/******/ 		// mode & 4: return value when already ns object
/******/ 		// mode & 16: return value when it's Promise-like
/******/ 		// mode & 8|1: behave like require
/******/ 		__webpack_require__.t = function(value, mode) {
/******/ 			if(mode & 1) value = this(value);
/******/ 			if(mode & 8) return value;
/******/ 			if(typeof value === 'object' && value) {
/******/ 				if((mode & 4) && value.__esModule) return value;
/******/ 				if((mode & 16) && typeof value.then === 'function') return value;
/******/ 			}
/******/ 			var ns = Object.create(null);
/******/ 			__webpack_require__.r(ns);
/******/ 			var def = {};
/******/ 			leafPrototypes = leafPrototypes || [null, getProto({}), getProto([]), getProto(getProto)];
/******/ 			for(var current = mode & 2 && value; (typeof current == 'object' || typeof current == 'function') && !~leafPrototypes.indexOf(current); current = getProto(current)) {
/******/ 				Object.getOwnPropertyNames(current).forEach((key) => (def[key] = () => (value[key])));
/******/ 			}
/******/ 			def['default'] = () => (value);
/******/ 			__webpack_require__.d(ns, def);
/******/ 			return ns;
/******/ 		};
/******/ 	})();
/******/ 	
/******/ 	/* webpack/runtime/define property getters */
/******/ 	(() => {
/******/ 		// define getter functions for harmony exports
/******/ 		__webpack_require__.d = (exports, definition) => {
/******/ 			for(var key in definition) {
/******/ 				if(__webpack_require__.o(definition, key) && !__webpack_require__.o(exports, key)) {
/******/ 					Object.defineProperty(exports, key, { enumerable: true, get: definition[key] });
/******/ 				}
/******/ 			}
/******/ 		};
/******/ 	})();
/******/ 	
/******/ 	/* webpack/runtime/hasOwnProperty shorthand */
/******/ 	(() => {
/******/ 		__webpack_require__.o = (obj, prop) => (Object.prototype.hasOwnProperty.call(obj, prop))
/******/ 	})();
/******/ 	
/******/ 	/* webpack/runtime/make namespace object */
/******/ 	(() => {
/******/ 		// define __esModule on exports
/******/ 		__webpack_require__.r = (exports) => {
/******/ 			if(typeof Symbol !== 'undefined' && Symbol.toStringTag) {
/******/ 				Object.defineProperty(exports, Symbol.toStringTag, { value: 'Module' });
/******/ 			}
/******/ 			Object.defineProperty(exports, '__esModule', { value: true });
/******/ 		};
/******/ 	})();
/******/ 	
/************************************************************************/
var __webpack_exports__ = {};
// This entry needs to be wrapped in an IIFE because it needs to be in strict mode.
(() => {
"use strict";
/*!*******************!*\
  !*** ./index.mjs ***!
  \*******************/
/* harmony import */ var _cjs_js__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./cjs.js */ 0);
/* harmony import */ var _reexport_mjs__WEBPACK_IMPORTED_MODULE_1__ = __webpack_require__(/*! ./reexport.mjs */ 4);
/* harmony import */ var _flagged_js__WEBPACK_IMPORTED_MODULE_2__ = __webpack_require__(/*! ./flagged.js */ 1);
/* harmony import */ var _dynamic_js__WEBPACK_IMPORTED_MODULE_3__ = __webpack_require__(/*! ./dynamic.js */ 2);
/* harmony import */ var _dynamicFlagged_js__WEBPACK_IMPORTED_MODULE_4__ = __webpack_require__(/*! ./dynamicFlagged.js */ 3);














console.log(
	_cjs_js__WEBPACK_IMPORTED_MODULE_0__.data, _cjs_js__WEBPACK_IMPORTED_MODULE_0__, _cjs_js__WEBPACK_IMPORTED_MODULE_0__, _reexport_mjs__WEBPACK_IMPORTED_MODULE_1__.ns, _reexport_mjs__WEBPACK_IMPORTED_MODULE_1__["default"], _reexport_mjs__WEBPACK_IMPORTED_MODULE_1__.def, _reexport_mjs__WEBPACK_IMPORTED_MODULE_1__.data, _reexport_mjs__WEBPACK_IMPORTED_MODULE_1__,
	_flagged_js__WEBPACK_IMPORTED_MODULE_2__.data, _flagged_js__WEBPACK_IMPORTED_MODULE_2__, _flagged_js__WEBPACK_IMPORTED_MODULE_2__,
	_dynamic_js__WEBPACK_IMPORTED_MODULE_3__.data, _dynamic_js__WEBPACK_IMPORTED_MODULE_3__, _dynamic_js__WEBPACK_IMPORTED_MODULE_3__,
	_dynamicFlagged_js__WEBPACK_IMPORTED_MODULE_4__.data, _dynamicFlagged_js__WEBPACK_IMPORTED_MODULE_4__, _dynamicFlagged_js__WEBPACK_IMPORTED_MODULE_4__
);

})();

/******/ })()
;
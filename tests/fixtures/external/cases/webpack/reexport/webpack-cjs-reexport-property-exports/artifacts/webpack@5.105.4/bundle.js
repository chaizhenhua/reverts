/******/ (() => { // webpackBootstrap
/******/ 	var __webpack_modules__ = ([
/* 0 */
/*!******************!*\
  !*** ./index.js ***!
  \******************/
/***/ ((module, __unused_webpack_exports, __webpack_require__) => {

const props = __webpack_require__(/*! ./reexport-property-exports */ 2);
console.log(props.property1, props.property2, props.property3, props.property4);
module.exports = props;

/***/ }),
/* 1 */
/*!*******************!*\
  !*** ./module.js ***!
  \*******************/
/***/ ((__unused_webpack_module, exports) => {

exports.abc = "abc";
exports.def = "def";

/***/ }),
/* 2 */
/*!**************************************!*\
  !*** ./reexport-property-exports.js ***!
  \**************************************/
/***/ (function(__unused_webpack_module, exports, __webpack_require__) {

exports.property1 = __webpack_require__(/*! ./module */ 1).abc;
var m2 = __webpack_require__(/*! ./module */ 1);
exports.property2 = m2.abc;
this.property3 = __webpack_require__(/*! ./module */ 1).abc;
var m4 = __webpack_require__(/*! ./module */ 1);
this.property4 = m4.abc;

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
/******/ 		__webpack_modules__[moduleId].call(module.exports, module, module.exports, __webpack_require__);
/******/ 	
/******/ 		// Return the exports of the module
/******/ 		return module.exports;
/******/ 	}
/******/ 	
/************************************************************************/
/******/ 	
/******/ 	// startup
/******/ 	// Load entry module and return exports
/******/ 	// This entry module is referenced by other modules so it can't be inlined
/******/ 	var __webpack_exports__ = __webpack_require__(0);
/******/ 	
/******/ })()
;
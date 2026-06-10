/******/ (() => { // webpackBootstrap
/******/ 	var __webpack_modules__ = ([
/* 0 */
/*!******************!*\
  !*** ./index.js ***!
  \******************/
/***/ ((module, __unused_webpack_exports, __webpack_require__) => {

const all = __webpack_require__(/*! ./reexport-whole-exports */ 2);
console.log(all.module1.abc, all.module2.abc, all.module3.abc, all.module4.abc);
module.exports = all;

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
/*!***********************************!*\
  !*** ./reexport-whole-exports.js ***!
  \***********************************/
/***/ (function(__unused_webpack_module, exports, __webpack_require__) {

exports.module1 = __webpack_require__(/*! ./module */ 1);
var m2 = __webpack_require__(/*! ./module */ 1);
exports.module2 = m2;
this.module3 = __webpack_require__(/*! ./module */ 1);
var m4 = __webpack_require__(/*! ./module */ 1);
this.module4 = m4;

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
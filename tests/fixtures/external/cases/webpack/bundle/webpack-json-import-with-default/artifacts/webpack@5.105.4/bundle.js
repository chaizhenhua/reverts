/******/ (() => { // webpackBootstrap
/******/ 	"use strict";
/******/ 	var __webpack_modules__ = ([
/* 0 */,
/* 1 */
/*!****************!*\
  !*** ./a.json ***!
  \****************/
/***/ ((module) => {

module.exports = null;

/***/ }),
/* 2 */
/*!****************!*\
  !*** ./b.json ***!
  \****************/
/***/ ((module) => {

module.exports = 123;

/***/ }),
/* 3 */
/*!****************!*\
  !*** ./c.json ***!
  \****************/
/***/ ((module) => {

module.exports = [0,0,0,0];

/***/ }),
/* 4 */
/*!****************!*\
  !*** ./e.json ***!
  \****************/
/***/ ((module) => {

module.exports = {"aa":1};

/***/ }),
/* 5 */
/*!****************!*\
  !*** ./f.json ***!
  \****************/
/***/ ((module) => {

module.exports = /*#__PURE__*/JSON.parse('{"KT":"named","Ay":"default"}');

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
// This entry needs to be wrapped in an IIFE because it needs to be isolated against other modules in the chunk.
(() => {
/*!******************!*\
  !*** ./index.js ***!
  \******************/
/* harmony import */ var _a_json__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./a.json */ 1);
/* harmony import */ var _b_json__WEBPACK_IMPORTED_MODULE_1__ = __webpack_require__(/*! ./b.json */ 2);
/* harmony import */ var _c_json__WEBPACK_IMPORTED_MODULE_2__ = __webpack_require__(/*! ./c.json */ 3);
/* harmony import */ var _e_json__WEBPACK_IMPORTED_MODULE_3__ = __webpack_require__(/*! ./e.json */ 4);
/* harmony import */ var _f_json__WEBPACK_IMPORTED_MODULE_4__ = __webpack_require__(/*! ./f.json */ 5);





console.log(_a_json__WEBPACK_IMPORTED_MODULE_0__, _b_json__WEBPACK_IMPORTED_MODULE_1__, _c_json__WEBPACK_IMPORTED_MODULE_2__.length, _e_json__WEBPACK_IMPORTED_MODULE_3__.aa, _f_json__WEBPACK_IMPORTED_MODULE_4__/* .named */ .KT, _f_json__WEBPACK_IMPORTED_MODULE_4__/* ["default"] */ .Ay);

})();

/******/ })()
;
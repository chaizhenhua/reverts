/******/ (() => { // webpackBootstrap
/******/ 	"use strict";
/******/ 	var __webpack_modules__ = ([
/* 0 */
/*!**********************!*\
  !*** ./leftHelix.js ***!
  \**********************/
/***/ ((__unused_webpack_module, __webpack_exports__, __webpack_require__) => {

__webpack_require__.r(__webpack_exports__);
/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   "default": () => (__WEBPACK_DEFAULT_EXPORT__)
/* harmony export */ });
/* harmony import */ var _leftHelixPrime__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./leftHelixPrime */ 1);

/* harmony default export */ const __WEBPACK_DEFAULT_EXPORT__ = ({ leftHelixPrime: _leftHelixPrime__WEBPACK_IMPORTED_MODULE_0__/* ["default"] */ .A, run: _leftHelixPrime__WEBPACK_IMPORTED_MODULE_0__/* .run */ .e });

/***/ }),
/* 1 */
/*!***************************!*\
  !*** ./leftHelixPrime.js ***!
  \***************************/
/***/ ((__unused_webpack_module, __webpack_exports__, __webpack_require__) => {

/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   A: () => (__WEBPACK_DEFAULT_EXPORT__),
/* harmony export */   e: () => (/* binding */ run)
/* harmony export */ });
/* harmony import */ var _rightHelixPrime__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./rightHelixPrime */ 2);

function run() {
	return Promise.resolve(/*! import() */).then(__webpack_require__.bind(__webpack_require__, /*! ./leftHelix */ 0));
}
/* harmony default export */ const __WEBPACK_DEFAULT_EXPORT__ = ({ rightHelixPrime: () => _rightHelixPrime__WEBPACK_IMPORTED_MODULE_0__/* ["default"] */ .A });

/***/ }),
/* 2 */
/*!****************************!*\
  !*** ./rightHelixPrime.js ***!
  \****************************/
/***/ ((__unused_webpack_module, __webpack_exports__, __webpack_require__) => {

/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   A: () => (__WEBPACK_DEFAULT_EXPORT__),
/* harmony export */   e: () => (/* binding */ run)
/* harmony export */ });
/* harmony import */ var _leftHelixPrime__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./leftHelixPrime */ 1);

function run() {
	return Promise.resolve(/*! import() */).then(__webpack_require__.bind(__webpack_require__, /*! ./rightHelix */ 3));
}
/* harmony default export */ const __WEBPACK_DEFAULT_EXPORT__ = ({ leftHelixPrime: () => _leftHelixPrime__WEBPACK_IMPORTED_MODULE_0__/* ["default"] */ .A });

/***/ }),
/* 3 */
/*!***********************!*\
  !*** ./rightHelix.js ***!
  \***********************/
/***/ ((__unused_webpack_module, __webpack_exports__, __webpack_require__) => {

__webpack_require__.r(__webpack_exports__);
/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   "default": () => (__WEBPACK_DEFAULT_EXPORT__)
/* harmony export */ });
/* harmony import */ var _rightHelixPrime__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./rightHelixPrime */ 2);

/* harmony default export */ const __WEBPACK_DEFAULT_EXPORT__ = ({ rightHelixPrime: _rightHelixPrime__WEBPACK_IMPORTED_MODULE_0__/* ["default"] */ .A, run: _rightHelixPrime__WEBPACK_IMPORTED_MODULE_0__/* .run */ .e });

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
// This entry needs to be wrapped in an IIFE because it needs to be isolated against other modules in the chunk.
(() => {
/*!******************!*\
  !*** ./index.js ***!
  \******************/
/* harmony import */ var _leftHelix__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./leftHelix */ 0);
/* harmony import */ var _rightHelix__WEBPACK_IMPORTED_MODULE_1__ = __webpack_require__(/*! ./rightHelix */ 3);


Promise.all([_leftHelix__WEBPACK_IMPORTED_MODULE_0__["default"].run(), _rightHelix__WEBPACK_IMPORTED_MODULE_1__["default"].run()]).then(values => {
	console.log(values.length, _leftHelix__WEBPACK_IMPORTED_MODULE_0__["default"].leftHelixPrime, _rightHelix__WEBPACK_IMPORTED_MODULE_1__["default"].rightHelixPrime);
});
/* unused harmony default export */ var __WEBPACK_DEFAULT_EXPORT__ = ({ leftHelix: _leftHelix__WEBPACK_IMPORTED_MODULE_0__["default"], rightHelix: _rightHelix__WEBPACK_IMPORTED_MODULE_1__["default"] });
})();

/******/ })()
;
/******/ (() => { // webpackBootstrap
/******/ 	"use strict";
/******/ 	var __webpack_modules__ = ({

/***/ 602
/*!********************!*\
  !*** ./module1.js ***!
  \********************/
(__unused_webpack_module, __webpack_exports__, __webpack_require__) {

/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   obj1: () => (/* binding */ obj1)
/* harmony export */ });
const obj1 = {};
/* unused harmony default export */ var __WEBPACK_DEFAULT_EXPORT__ = ({ obj2: {} });

/***/ },

/***/ 503
/*!********************!*\
  !*** ./module2.js ***!
  \********************/
(__unused_webpack_module, __webpack_exports__, __webpack_require__) {

/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   m_1: () => (/* reexport module object */ _module1__WEBPACK_IMPORTED_MODULE_0__)
/* harmony export */ });
/* harmony import */ var _module1__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./module1 */ 602);



/***/ },

/***/ 144
/*!********************!*\
  !*** ./module3.js ***!
  \********************/
(__unused_webpack_module, __webpack_exports__, __webpack_require__) {

/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   m_2: () => (/* reexport module object */ _module2__WEBPACK_IMPORTED_MODULE_0__)
/* harmony export */ });
/* harmony import */ var _module2__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./module2 */ 503);



/***/ },

/***/ 306
/*!*******************!*\
  !*** ./data.json ***!
  \*******************/
(module) {

module.exports = {"aa":1};

/***/ }

/******/ 	});
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
/************************************************************************/
var __webpack_exports__ = {};
/*!******************!*\
  !*** ./index.js ***!
  \******************/
/* harmony import */ var _module1__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./module1 */ 602);
/* harmony import */ var _module2__WEBPACK_IMPORTED_MODULE_1__ = __webpack_require__(/*! ./module2 */ 503);
/* harmony import */ var _module3__WEBPACK_IMPORTED_MODULE_2__ = __webpack_require__(/*! ./module3 */ 144);
/* harmony import */ var _data_json__WEBPACK_IMPORTED_MODULE_3__ = __webpack_require__(/*! ./data.json */ 306);




console.log(_module1__WEBPACK_IMPORTED_MODULE_0__.obj1, _module2__WEBPACK_IMPORTED_MODULE_1__.m_1.obj1, _module3__WEBPACK_IMPORTED_MODULE_2__.m_2.m_1.obj1, _data_json__WEBPACK_IMPORTED_MODULE_3__.aa);

/******/ })()
;
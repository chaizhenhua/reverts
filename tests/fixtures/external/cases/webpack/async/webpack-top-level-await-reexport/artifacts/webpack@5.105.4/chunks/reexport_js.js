"use strict";
exports.id = "reexport_js";
exports.ids = ["reexport_js"];
exports.modules = [
/* 0 */,
/* 1 */
/*!*********************!*\
  !*** ./reexport.js ***!
  \*********************/
/***/ ((module, __webpack_exports__, __webpack_require__) => {

__webpack_require__.a(module, async (__webpack_handle_async_dependencies__, __webpack_async_result__) => { try {
/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   "default": () => (/* reexport safe */ _module__WEBPACK_IMPORTED_MODULE_0__.A),
/* harmony export */   other: () => (/* binding */ other)
/* harmony export */ });
/* harmony import */ var _module__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./module */ 2);
var __webpack_async_dependencies__ = __webpack_handle_async_dependencies__([_module__WEBPACK_IMPORTED_MODULE_0__]);
var __webpack_async_dependencies_result__ = (__webpack_async_dependencies__.then ? (await __webpack_async_dependencies__)() : __webpack_async_dependencies__);
_module__WEBPACK_IMPORTED_MODULE_0__ = __webpack_async_dependencies_result__[0];


const other = _module__WEBPACK_IMPORTED_MODULE_0__/* ["default"] */ .A;
__webpack_async_result__();
} catch(e) { __webpack_async_result__(e); } });

/***/ }),
/* 2 */
/*!*******************!*\
  !*** ./module.js ***!
  \*******************/
/***/ ((module, __webpack_exports__, __webpack_require__) => {

__webpack_require__.a(module, async (__webpack_handle_async_dependencies__, __webpack_async_result__) => { try {
/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   A: () => (__WEBPACK_DEFAULT_EXPORT__)
/* harmony export */ });
await new Promise(r => setTimeout(r, 1));
/* harmony default export */ const __WEBPACK_DEFAULT_EXPORT__ = (42);
__webpack_async_result__();
} catch(e) { __webpack_async_result__(e); } }, 1);

/***/ })
];
;
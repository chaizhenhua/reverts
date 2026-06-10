exports.id = "test_js";
exports.ids = ["test_js"];
exports.modules = [
/* 0 */,
/* 1 */
/*!*****************!*\
  !*** ./test.js ***!
  \*****************/
/***/ ((module, __webpack_exports__, __webpack_require__) => {

"use strict";
__webpack_require__.a(module, async (__webpack_handle_async_dependencies__, __webpack_async_result__) => { try {
/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   a: () => (/* reexport safe */ _reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.a),
/* harmony export */   b: () => (/* reexport safe */ _reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.b),
/* harmony export */   c: () => (/* reexport safe */ _reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.c)
/* harmony export */ });
/* harmony import */ var _reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./reexport-async-unknown.js */ 2);
var __webpack_async_dependencies__ = __webpack_handle_async_dependencies__([_reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__]);
var __webpack_async_dependencies_result__ = (__webpack_async_dependencies__.then ? (await __webpack_async_dependencies__)() : __webpack_async_dependencies__);
_reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__ = __webpack_async_dependencies_result__[0];



console.log(_reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__["default"], _reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.a, _reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.b, _reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.c, _reexport_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__["default"]);

__webpack_async_result__();
} catch(e) { __webpack_async_result__(e); } });

/***/ }),
/* 2 */
/*!***********************************!*\
  !*** ./reexport-async-unknown.js ***!
  \***********************************/
/***/ ((module, __webpack_exports__, __webpack_require__) => {

"use strict";
__webpack_require__.a(module, async (__webpack_handle_async_dependencies__, __webpack_async_result__) => { try {
/* harmony export */ __webpack_require__.d(__webpack_exports__, {
/* harmony export */   a: () => (/* reexport safe */ _async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.a),
/* harmony export */   "default": () => (__WEBPACK_DEFAULT_EXPORT__)
/* harmony export */ });
/* harmony import */ var _async_unknown_js__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./async-unknown.js */ 3);
var __webpack_async_dependencies__ = __webpack_handle_async_dependencies__([_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__]);
var __webpack_async_dependencies_result__ = (__webpack_async_dependencies__.then ? (await __webpack_async_dependencies__)() : __webpack_async_dependencies__);
_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__ = __webpack_async_dependencies_result__[0];
/* harmony reexport (checked) */ if(__webpack_require__.o(_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__, "b")) __webpack_require__.d(__webpack_exports__, { b: function() { return _async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.b; } });
/* harmony reexport (checked) */ if(__webpack_require__.o(_async_unknown_js__WEBPACK_IMPORTED_MODULE_0__, "c")) __webpack_require__.d(__webpack_exports__, { c: function() { return _async_unknown_js__WEBPACK_IMPORTED_MODULE_0__.c; } });


/* harmony default export */ const __WEBPACK_DEFAULT_EXPORT__ = ("default");
__webpack_async_result__();
} catch(e) { __webpack_async_result__(e); } });

/***/ }),
/* 3 */
/*!**************************!*\
  !*** ./async-unknown.js ***!
  \**************************/
/***/ ((module, __webpack_exports__, __webpack_require__) => {

"use strict";
__webpack_require__.a(module, async (__webpack_handle_async_dependencies__, __webpack_async_result__) => { try {
/* harmony import */ var _unknown_js__WEBPACK_IMPORTED_MODULE_0__ = __webpack_require__(/*! ./unknown.js */ 4);
/* harmony import */ var _unknown_js__WEBPACK_IMPORTED_MODULE_0___default = /*#__PURE__*/__webpack_require__.n(_unknown_js__WEBPACK_IMPORTED_MODULE_0__);
/* harmony reexport (checked) */ if(__webpack_require__.o(_unknown_js__WEBPACK_IMPORTED_MODULE_0__, "a")) __webpack_require__.d(__webpack_exports__, { a: function() { return _unknown_js__WEBPACK_IMPORTED_MODULE_0__.a; } });
/* harmony reexport (checked) */ if(__webpack_require__.o(_unknown_js__WEBPACK_IMPORTED_MODULE_0__, "b")) __webpack_require__.d(__webpack_exports__, { b: function() { return _unknown_js__WEBPACK_IMPORTED_MODULE_0__.b; } });
/* harmony reexport (checked) */ if(__webpack_require__.o(_unknown_js__WEBPACK_IMPORTED_MODULE_0__, "c")) __webpack_require__.d(__webpack_exports__, { c: function() { return _unknown_js__WEBPACK_IMPORTED_MODULE_0__.c; } });

await 1;
__webpack_async_result__();
} catch(e) { __webpack_async_result__(e); } }, 1);

/***/ }),
/* 4 */
/*!********************!*\
  !*** ./unknown.js ***!
  \********************/
/***/ ((module) => {

const o = {
	a: "a",
	b: "b",
	c: "c"
};
module.exports = Object(o);

/***/ })
];
;
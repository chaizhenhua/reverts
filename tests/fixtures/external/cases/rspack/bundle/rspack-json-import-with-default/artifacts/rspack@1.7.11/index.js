(() => {
"use strict";
var __webpack_modules__ = ({
"./data/a.json"
/*!*********************!*\
  !*** ./data/a.json ***!
  \*********************/
(module) {
module.exports = null

},
"./data/b.json"
/*!*********************!*\
  !*** ./data/b.json ***!
  \*********************/
(module) {
module.exports = 123

},
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
__webpack_require__.r(__webpack_exports__);
/* import */ var _data_a_json__rspack_import_0 = __webpack_require__(/*! ./data/a.json */ "./data/a.json");
/* import */ var _data_b_json__rspack_import_1 = __webpack_require__(/*! ./data/b.json */ "./data/b.json");
/* import */ var _data_c_json__rspack_import_2 = __webpack_require__(/*! ./data/c.json */ "./data/c.json");
/* import */ var _data_d_json__rspack_import_3 = __webpack_require__(/*! ./data/d.json */ "./data/d.json");
/* import */ var _data_e_json__rspack_import_4 = __webpack_require__(/*! ./data/e.json */ "./data/e.json");
/* import */ var _data_f_json__rspack_import_5 = __webpack_require__(/*! ./data/f.json */ "./data/f.json");






console.log(_data_a_json__rspack_import_0, _data_b_json__rspack_import_1, _data_c_json__rspack_import_2, _data_d_json__rspack_import_3, _data_e_json__rspack_import_4, _data_f_json__rspack_import_5);

})();

})()
;
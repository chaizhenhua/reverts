"use strict";
exports.ids = ["513"];
exports.modules = {
"./UserApi.js"
/*!********************!*\
  !*** ./UserApi.js ***!
  \********************/
(module, __webpack_exports__, __webpack_require__) {
__webpack_require__.a(module, async function (__rspack_load_async_deps, __rspack_async_done) { try {
__webpack_require__.r(__webpack_exports__);
__webpack_require__.d(__webpack_exports__, {
  createUser: () => (createUser)
});
/* import */ var _db_connection_js__rspack_import_0 = __webpack_require__(/*! ./db-connection.js */ "./db-connection.js");
var __rspack_async_deps = __rspack_load_async_deps([_db_connection_js__rspack_import_0]);
_db_connection_js__rspack_import_0 = (__rspack_async_deps.then ? (await __rspack_async_deps)() : __rspack_async_deps)[0];

const createUser = async name => {
	command = `CREATE USER ${name}`;
	// This is a normal await, because it's in an async function
	await (0,_db_connection_js__rspack_import_0.dbCall)({ command });
};

__rspack_async_done();
} catch(e) { __rspack_async_done(e); } });

},
"./db-connection.js"
/*!**************************!*\
  !*** ./db-connection.js ***!
  \**************************/
(module, __webpack_exports__, __webpack_require__) {
__webpack_require__.a(module, async function (__rspack_load_async_deps, __rspack_async_done) { try {
__webpack_require__.r(__webpack_exports__);
__webpack_require__.d(__webpack_exports__, {
  close: () => (close),
  dbCall: () => (dbCall)
});
const connectToDB = async url => {
	await new Promise(r => setTimeout(r, 1000));
};

// This is a top-level-await
await connectToDB("my-sql://example.com");

const dbCall = async data => {
	// This is a normal await, because it's in an async function
	await new Promise(r => setTimeout(r, 100));
	return "fake data";
};

const close = () => {
	console.log("closes the DB connection");
};

__rspack_async_done();
} catch(e) { __rspack_async_done(e); } }, 1);

},

};
;
(() => {
var __webpack_modules__ = ({});
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

// expose the modules object (__webpack_modules__)
__webpack_require__.m = __webpack_modules__;

// webpack/runtime/create_fake_namespace_object
(() => {
var getProto = Object.getPrototypeOf ? (obj) => (Object.getPrototypeOf(obj)) : (obj) => (obj.__proto__);
var leafPrototypes;
// create a fake namespace object
// mode & 1: value is a module id, require it
// mode & 2: merge all properties of value into the ns
// mode & 4: return value when already ns object
// mode & 16: return value when it's Promise-like
// mode & 8|1: behave like require
__webpack_require__.t = function(value, mode) {
	if(mode & 1) value = this(value);
	if(mode & 8) return value;
	if(typeof value === 'object' && value) {
		if((mode & 4) && value.__esModule) return value;
		if((mode & 16) && typeof value.then === 'function') return value;
	}
	var ns = Object.create(null);
  __webpack_require__.r(ns);
	var def = {};
	leafPrototypes = leafPrototypes || [null, getProto({}), getProto([]), getProto(getProto)];
	for(var current = mode & 2 && value; (typeof current == 'object' || typeof current == 'function') && !~leafPrototypes.indexOf(current); current = getProto(current)) {
		Object.getOwnPropertyNames(current).forEach((key) => { def[key] = () => (value[key]) });
	}
	def['default'] = () => (value);
	__webpack_require__.d(ns, def);
	return ns;
};
})();
// webpack/runtime/define_property_getters
(() => {
__webpack_require__.d = (exports, definition) => {
	for(var key in definition) {
        if(__webpack_require__.o(definition, key) && !__webpack_require__.o(exports, key)) {
            Object.defineProperty(exports, key, { enumerable: true, get: definition[key] });
        }
    }
};
})();
// webpack/runtime/ensure_chunk
(() => {
__webpack_require__.f = {};
// This file contains only the entry chunk.
// The chunk loading function for additional chunks
__webpack_require__.e = (chunkId) => {
	return Promise.all(
		Object.keys(__webpack_require__.f).reduce((promises, key) => {
			__webpack_require__.f[key](chunkId, promises);
			return promises;
		}, [])
	);
};
})();
// webpack/runtime/get javascript chunk filename
(() => {
// This function allow to reference chunks
__webpack_require__.u = (chunkId) => {
  // return url for filenames not based on template
  
  // return url for filenames based on template
  return "chunks/" + chunkId + ".js"
}
})();
// webpack/runtime/has_own_property
(() => {
__webpack_require__.o = (obj, prop) => (Object.prototype.hasOwnProperty.call(obj, prop))
})();
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
// webpack/runtime/require_chunk_loading
(() => {
var installedChunks = {"index": 1,};
// object to store loaded chunks
// "1" means "loaded", otherwise not loaded yet
var installChunk = (chunk) => {
	var moreModules = chunk.modules, chunkIds = chunk.ids, runtime = chunk.runtime;
	for (var moduleId in moreModules) {
		if (__webpack_require__.o(moreModules, moduleId)) {
		 __webpack_require__.m[moduleId] = moreModules[moduleId];
		}
	}
	if (runtime) runtime(__webpack_require__);
	for (var i = 0; i < chunkIds.length; i++) installedChunks[chunkIds[i]] = 1;
	
};// require() chunk loading for javascript
__webpack_require__.f.require = (chunkId, promises) => {
	// "1" is the signal for "already loaded"
	if (!installedChunks[chunkId]) {
		if (true) {
			installChunk(require("./" + __webpack_require__.u(chunkId)));
		} else installedChunks[chunkId] = 1;
	}
};
})();
// webpack/runtime/rspack_unique_id
(() => {
__webpack_require__.ruid = "bundler=rspack@1.7.11";
})();

/*!******************!*\
  !*** ./index.js ***!
  \******************/
Promise.all([
  __webpack_require__.e(/*! import() */ "data_a_json").then(__webpack_require__.t.bind(__webpack_require__, /*! ./data/a.json */ "./data/a.json", 17)),
  __webpack_require__.e(/*! import() */ "data_b_json").then(__webpack_require__.t.bind(__webpack_require__, /*! ./data/b.json */ "./data/b.json", 17)),
  __webpack_require__.e(/*! import() */ "data_c_json").then(__webpack_require__.t.bind(__webpack_require__, /*! ./data/c.json */ "./data/c.json", 19)),
  __webpack_require__.e(/*! import() */ "data_d_json").then(__webpack_require__.t.bind(__webpack_require__, /*! ./data/d.json */ "./data/d.json", 19)),
  __webpack_require__.e(/*! import() */ "data_e_json").then(__webpack_require__.t.bind(__webpack_require__, /*! ./data/e.json */ "./data/e.json", 19)),
  __webpack_require__.e(/*! import() */ "data_f_json").then(__webpack_require__.t.bind(__webpack_require__, /*! ./data/f.json */ "./data/f.json", 19)),
  __webpack_require__.e(/*! import() */ "data_g_json").then(__webpack_require__.t.bind(__webpack_require__, /*! ./data/g.json */ "./data/g.json", 19)),
]).then(values => console.log(values));

})()
;
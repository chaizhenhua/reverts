(() => {
"use strict";
var __webpack_modules__ = ({
"./Actions.js"
/*!********************!*\
  !*** ./Actions.js ***!
  \********************/
(__unused_rspack_module, __webpack_exports__, __webpack_require__) {
__webpack_require__.r(__webpack_exports__);
__webpack_require__.d(__webpack_exports__, {
  AlternativeCreateUserAction: () => (AlternativeCreateUserAction),
  CreateUserAction: () => (CreateUserAction)
});
// import() doesn't care about whether a module is an async module or not
const UserApi = __webpack_require__.e(/*! import() */ "513").then(__webpack_require__.bind(__webpack_require__, /*! ./UserApi.js */ "./UserApi.js"));

const CreateUserAction = async name => {
	// These are normal awaits, because they are in an async function
	const { createUser } = await UserApi;
	await createUser(name);
};

// You can place import() where you like
// Placing it at top-level will start loading and evaluating on
//   module evaluation.
//   see CreateUserAction above
//   Here: Connecting to the DB starts when the application starts
// Placing it inside of an (async) function will start loading
//   and evaluating when the function is called for the first time
//   which basically makes it lazy-loaded.
//   see AlternativeCreateUserAction below
//   Here: Connecting to the DB starts when AlternativeCreateUserAction
//         is called
const AlternativeCreateUserAction = async name => {
	const { createUser } = await __webpack_require__.e(/*! import() */ "513").then(__webpack_require__.bind(__webpack_require__, /*! ./UserApi.js */ "./UserApi.js"));
	await createUser(name);
};

// Note: Using await import() at top-level doesn't make much sense
//       except in rare cases. It will import modules sequentially.


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

// expose the modules object (__webpack_modules__)
__webpack_require__.m = __webpack_modules__;

// webpack/runtime/async_module
(() => {
var hasSymbol = typeof Symbol === "function";
var rspackQueues = hasSymbol ? Symbol("rspack queues") : "__rspack_queues";
var rspackExports = __webpack_require__.aE = hasSymbol ? Symbol("rspack exports") : "__webpack_exports__";
var rspackError = hasSymbol ? Symbol("rspack error") : "__rspack_error";
var rspackDone = hasSymbol ? Symbol("rspack done") : "__rspack_done";
var rspackDefer = __webpack_require__.zS = hasSymbol ? Symbol("rspack defer") : "__rspack_defer";
var resolveQueue = (queue) => {
  if (queue && queue.d < 1) {
    queue.d = 1;
    queue.forEach((fn) => (fn.r--));
		queue.forEach((fn) => (fn.r-- ? fn.r++ : fn()));
	}
}
var wrapDeps = (deps) => {
	return deps.map((dep) => {
		if (dep !== null && typeof dep === "object") {
			if(!dep[rspackQueues] && dep[rspackDefer]) {
				var asyncDeps = dep[rspackDefer];
				var hasUnresolvedAsyncSubgraph = asyncDeps.some((id) => {
					var cache = __webpack_module_cache__[id];
					return !cache || cache[rspackDone] === false;
				});
				if (hasUnresolvedAsyncSubgraph) {
					var d = dep;
					dep = {
						then(callback) {
							Promise.all(asyncDeps.map(__webpack_require__)).then(() => (callback(d)))
						}
					};
				} else return dep;
			}
			if (dep[rspackQueues]) return dep;
			if (dep.then) {
				var queue = [];
				queue.d = 0;
				dep.then((r) => {
					obj[rspackExports] = r;
					resolveQueue(queue);
				},(e) => {
					obj[rspackError] = e;
					resolveQueue(queue);
				});
				var obj = {};
				obj[rspackDefer] = false;
				obj[rspackQueues] = (fn) => (fn(queue));
				return obj;
			}
		}
		var ret = {};
		ret[rspackQueues] = () => {};
		ret[rspackExports] = dep;
		return ret;
	});
};
__webpack_require__.a = (module, body, hasAwait) => {
	var queue;
	hasAwait && ((queue = []).d = -1);
	var depQueues = new Set();
	var exports = module.exports;
	var currentDeps;
	var outerResolve;
	var reject;
	var promise = new Promise((resolve, rej) => {
		reject = rej;
		outerResolve = resolve;
	});
	promise[rspackExports] = exports;
	promise[rspackQueues] = (fn) => { queue && fn(queue), depQueues.forEach(fn), promise["catch"](() => {}); };
	module.exports = promise;
	var handle = (deps) => {
		currentDeps = wrapDeps(deps);
		var fn;
		var getResult = () => {
			return currentDeps.map((d) => {
				if(d[rspackDefer]) return d;
				if (d[rspackError]) throw d[rspackError];
				return d[rspackExports];
			});
		}
		var promise = new Promise((resolve) => {
			fn = () => (resolve(getResult));
			fn.r = 0;
			var fnQueue = (q) => (q !== queue && !depQueues.has(q) && (depQueues.add(q), q && !q.d && (fn.r++, q.push(fn))));
			currentDeps.map((dep) => (dep[rspackDefer] || dep[rspackQueues](fnQueue)));
		});
		return fn.r ? promise : getResult();
	};
	var done = (err) => ((err ? reject(promise[rspackError] = err) : outerResolve(exports)), resolveQueue(queue), promise[rspackDone] = true);
	body(handle, done);
	queue && queue.d < 0 && (queue.d = 0);
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
var installedChunks = {"176": 1,};
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
var __webpack_exports__ = {};
// This entry needs to be wrapped in an IIFE because it needs to be isolated against other modules in the chunk.
(() => {

/*!********************!*\
  !*** ./example.js ***!
  \********************/
__webpack_require__.r(__webpack_exports__);
/* import */ var _Actions_js__rspack_import_0 = __webpack_require__(/*! ./Actions.js */ "./Actions.js");


(async ()=> {
	await (0,_Actions_js__rspack_import_0.CreateUserAction)("John");
})();

})();

})()
;
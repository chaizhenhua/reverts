//#region \0rolldown/runtime.js
var __defProp = Object.defineProperty;
var __exportAll = (all, no_symbols) => {
	let target = {};
	for (var name in all) __defProp(target, name, {
		get: all[name],
		enumerable: true
	});
	if (!no_symbols) __defProp(target, Symbol.toStringTag, { value: "Module" });
	return target;
};
//#endregion
//#region barrel/a.js
Promise.resolve().then(() => barrel_exports);
//#endregion
//#region barrel/index.js
var barrel_exports = /* @__PURE__ */ __exportAll({
	a: () => "a",
	b: () => "b"
});
//#endregion
//#region main.js
console.log("a");
//#endregion

const post = "dynamic.js";
export async function loadNamespace() {
	const [cjs, flagged, dynamic] = await Promise.all([
		import("./cjs.js"),
		import("./cjs-esmodule.js"),
		import(/* webpackMode: "eager" */ "./cjs-" + post)
	]);
	return { cjs, flagged, dynamic };
}
loadNamespace().then(result => {
	console.log(result.cjs.default, result.cjs.named, result.flagged.default, result.dynamic);
});
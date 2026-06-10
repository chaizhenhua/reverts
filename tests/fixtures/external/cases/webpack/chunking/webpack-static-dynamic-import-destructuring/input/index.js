(async () => {
	const { default: def, usedExports } = await import("./dir1/a");
	console.log(def, usedExports);
	const { a: { aaa, usedExports: usedExportsA }, b: { bbb, usedExports: usedExportsB } } = await import("./lib/index");
	console.log(aaa, bbb, usedExportsA, usedExportsB);
})();
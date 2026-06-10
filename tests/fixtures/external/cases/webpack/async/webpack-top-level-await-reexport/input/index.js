import("./reexport").then(mod => {
	console.log(mod.default, mod.other);
});
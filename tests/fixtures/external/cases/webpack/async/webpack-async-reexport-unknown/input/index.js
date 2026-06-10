import("./test").then(mod => {
	console.log(mod.a, mod.b, mod.c, mod.default);
});
import("./module").then(mod => {
	console.log(mod.__esModule, mod.default);
});
Promise.all([
	import("./namespace-via-exports"),
	import("./namespace-via-literal"),
	import("./namespace-via-define-property"),
	import("./namespace-via-define-properties")
]).then(values => {
	console.log(values.map(v => [v.abc, v.default]));
});
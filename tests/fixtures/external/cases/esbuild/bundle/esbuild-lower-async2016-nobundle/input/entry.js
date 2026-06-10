
				async function foo(bar) {
					await bar
					return [this, arguments]
				}
				class Foo {async foo() {}}
				new (class Bar extends class { } {
					constructor() {
						let x = 1;
						(async () => {
							console.log("before super", x);  // (1) Sync phase
							await 1;
							console.log("after super", x);   // (2) Async phase
						})();
						super();
						x = 2;
					}
				})();
				export default [
					foo,
					Foo,
					async function() {},
					async () => {},
					{async foo() {}},
					class {async foo() {}},
					function() {
						return async (bar) => {
							await bar
							return [this, arguments]
						}
					},
				]
			
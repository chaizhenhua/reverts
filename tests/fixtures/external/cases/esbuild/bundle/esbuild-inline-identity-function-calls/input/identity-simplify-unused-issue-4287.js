
				function id(x) { return x }
				id({ x: id([123, foo()]) })
				id({ x: id(123) })
			

				{
					const REMOVE_n = null
					const REMOVE_u = undefined
					const REMOVE_i = 1234567
					const REMOVE_f = 123.456
					const REMOVE_s = 'abc' // String inlining is intentionally not supported right now
					const s_keep = 'Long strings are not inlined as constants'
					console.log(
						// These are doubled to avoid the "inline const/let into next statement if used once" optimization
						REMOVE_n, REMOVE_n,
						REMOVE_u, REMOVE_u,
						REMOVE_i, REMOVE_i,
						REMOVE_f, REMOVE_f,
						REMOVE_s, REMOVE_s,
						s_keep, s_keep,
					)
				}
			
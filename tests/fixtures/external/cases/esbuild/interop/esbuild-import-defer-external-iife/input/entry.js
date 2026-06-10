
				import defer * as foo0 from './foo.json'
				import defer * as foo1 from './foo.json' with { type: 'json' }

				console.log(
					foo0,
					foo1,
					import.defer('./foo.json'),
					import.defer('./foo.json', { with: { type: 'json' } }),
					import.defer(`./${foo}.json`),
					import.defer(`./${foo}.json`, { with: { type: 'json' } }),
				)
			
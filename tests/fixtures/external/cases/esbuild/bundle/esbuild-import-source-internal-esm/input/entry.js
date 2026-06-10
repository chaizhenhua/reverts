
				import source foo0 from './foo.json'
				import source foo1 from './foo.json' with { type: 'json' }

				console.log(
					foo0,
					foo1,
					import.source('./foo.json'),
					import.source('./foo.json', { with: { type: 'json' } }),
					import.source(`./${foo}.json`),
					import.source(`./${foo}.json`, { with: { type: 'json' } }),
				)
			
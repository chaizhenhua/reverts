
				import { foo } from 'js-pkg'
				import { bar } from 'ts-pkg'
				import { foo as shimFoo, bar as shimBar } from 'pkg'
				if (foo !== 'foo') throw 'fail: foo'
				if (bar !== 'bar') throw 'fail: bar'
				if (shimFoo !== 'shimFoo') throw 'fail: shimFoo'
				if (shimBar !== 'shimBar') throw 'fail: shimBar'
			
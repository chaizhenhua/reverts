
				import * as empty_js from './empty.js'
				import * as empty_esm_js from './empty.esm.js'
				import * as empty_json from './empty.json'
				import * as empty_css from './empty.css'
				import * as empty_global_css from './empty.global-css'
				import * as empty_local_css from './empty.local-css'

				import * as pkg_empty_js from 'pkg/empty.js'
				import * as pkg_empty_esm_js from 'pkg/empty.esm.js'
				import * as pkg_empty_json from 'pkg/empty.json'
				import * as pkg_empty_css from 'pkg/empty.css'
				import * as pkg_empty_global_css from 'pkg/empty.global-css'
				import * as pkg_empty_local_css from 'pkg/empty.local-css'

				import 'pkg'

				console.log(
					empty_js.foo,
					empty_esm_js.foo,
					empty_json.foo,
					empty_css.foo,
					empty_global_css.foo,
					empty_local_css.foo,
				)

				console.log(
					pkg_empty_js.foo,
					pkg_empty_esm_js.foo,
					pkg_empty_json.foo,
					pkg_empty_css.foo,
					pkg_empty_global_css.foo,
					pkg_empty_local_css.foo,
				)
			
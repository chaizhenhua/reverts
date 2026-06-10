import * as data from './data.json' assert { type: 'json' };
import * as depData from '@vitejs/test-import-assertion-dep';
console.log(data.foo, depData.hello);
export { data, depData };
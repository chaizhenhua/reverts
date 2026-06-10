import { g as getInfo } from './lib.js';

function getInfoWithUsed() {
	return getInfo() + '_used';
}

export { getInfoWithUsed };

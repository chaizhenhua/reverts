import { g as getInfo } from './lib.js';

function getInfoWithVariant() {
	return getInfo() + '_variant';
}

export { getInfoWithVariant };

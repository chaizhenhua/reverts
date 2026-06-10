import * as m1 from "./module1";
import * as m2 from "./module2";
import * as m3 from "./module3";
import data from "./data.json";
console.log(m1.obj1, m2.m_1.obj1, m3.m_2.m_1.obj1, data.aa);
export { m1, m2, m3, data };
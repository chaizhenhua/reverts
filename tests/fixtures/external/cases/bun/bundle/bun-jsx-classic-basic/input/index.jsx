import { print } from "bun-test-helpers";
import * as React from "custom-classic";
print([<div props={123}>Hello World</div>, <>Fragment</>]);

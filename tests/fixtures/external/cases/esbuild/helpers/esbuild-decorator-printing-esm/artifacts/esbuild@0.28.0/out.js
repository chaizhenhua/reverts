// entry.js
import { imported } from "somewhere";
_ = class {
  #bar;
  classes = [
    class {
      @imported @imported() imported;
    },
    class {
      @unbound @unbound() unbound;
    },
    class {
      @(123) @(123()) constant;
    },
    class {
      @(void 0) @((void 0)()) undef;
    },
    class {
      @(element[access]) indexed;
    },
    class {
      @foo.#bar private;
    },
    class {
      @(foo["\u30FF"]) unicode;
    },
    class {
      @(() => {
      }) arrow;
    }
  ];
};

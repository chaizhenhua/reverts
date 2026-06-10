import a from "./a" with { a: "a", "b": "b" };

export async function f() {
    const a = import("./a", {
        with: { a: "a", "b": "b" },
    });
    a;
}


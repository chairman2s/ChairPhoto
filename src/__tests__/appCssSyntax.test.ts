import { transform } from "esbuild";
import { describe, expect, it } from "vitest";
import css from "../App.css?raw";

describe("App.css", () => {
  it("parses without syntax warnings", async () => {
    const result = await transform(css, { loader: "css", minify: true });

    expect(result.warnings).toEqual([]);
  });
});

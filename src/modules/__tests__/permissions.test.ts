/**
 * Per-module permissions on `api.invoke` (#48).
 *
 * `api.invoke` used to be `(command, args) => invoke(command, args)` — a raw pass-through
 * that let any enabled module call any Tauri command the app registers. It is now gated on
 * two things at once: the command must be DECLARED by the module and GRANTED by the user.
 * The tests below are organised around the ways that gate can be wrong:
 *
 *  - it lets an undeclared command through (the bug the issue is about);
 *  - it blocks a declared, granted one (the gate is useless if the modules that ship stop
 *    working — and bundled modules are deliberately not exempt);
 *  - it refuses silently, leaving a module looking broken with nothing said to anyone;
 *  - it keys on something a module supplies, so one module can borrow another's grant;
 *  - an external module's manifest says one thing and its code grants itself another;
 *  - a grant survives when it should not (a module that narrowed its declaration), or
 *    fails to survive when it should (the user's approval must persist across restarts);
 *  - a corrupt or missing settings row reads as "allow" rather than "ask again".
 *
 * The Tauri `invoke` boundary is mocked so a permitted call can be observed *arriving*, not
 * merely not-throwing: "the promise resolved" would also be true of a gate that swallowed
 * the call. The settings boundary is mocked as a real in-memory store, so persistence and
 * reload are exercised through `initHost` rather than asserted on a private helper.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  __resetForTests,
  declaredPermissions,
  disableModule,
  enableModule,
  grantedPermissions,
  grantPermissions,
  initHost,
  listModules,
  ModulePermissionError,
  pendingPermissions,
  register,
  revokePermissions,
  setToastSink,
} from "../host";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";

// The Tauri command boundary. Every permitted invoke lands here; every refused one must not.
const invoked: { command: string; args?: Record<string, unknown> }[] = [];
vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args?: Record<string, unknown>) => {
      // `invoked` is declared above but this factory is hoisted; it only runs when a test
      // calls invoke, by which time the array exists.
      invoked.push({ command, args });
      return Promise.resolve(`result of ${command}`);
    },
  };
});

// A real settings store, so "the grant is persisted" and "the grant is read back at
// startup" are one round trip rather than two assertions about the same private map.
const settings = new Map<string, string>();
vi.mock("../api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api")>();
  return {
    ...actual,
    listExternalModules: () => Promise.resolve([]),
    pluginFeatures: () => Promise.resolve([]),
    appVersion: () => Promise.resolve("2026.8.0"),
    getSetting: (key: string) => Promise.resolve(settings.get(key) ?? null),
    setSetting: (key: string, value: string) => {
      settings.set(key, value);
      return Promise.resolve();
    },
  };
});

// ── Helpers ──────────────────────────────────────────────────────────────────

/** Capture the host API a module is handed at onLoad — the only instance production uses. */
function makeModule(id: string, overrides: Partial<ChairPhotoModule> = {}): ChairPhotoModule {
  return {
    id,
    name: overrides.name ?? id,
    version: overrides.version ?? "1.0.0",
    permissions: overrides.permissions,
    requires: overrides.requires,
    onLoad: overrides.onLoad ?? (() => {}),
  };
}

/** Register + grant + enable, returning the module's injected API. The grant is explicit
 *  because that is the production sequence: the Modules panel reviews, then grants, then
 *  enables. */
function liveApi(mod: ChairPhotoModule): ChairPhotoAPI {
  let captured: ChairPhotoAPI | null = null;
  register({ ...mod, onLoad: (api) => (captured = api) });
  grantPermissions(mod.id, false);
  enableModule(mod.id, false);
  if (!captured) throw new Error(`${mod.id} was not enabled — onLoad never ran`);
  return captured;
}

const toasts: string[] = [];

beforeEach(() => {
  invoked.length = 0;
  toasts.length = 0;
  settings.clear();
  setToastSink((m) => toasts.push(m));
  vi.spyOn(console, "warn").mockImplementation(() => {});
  vi.spyOn(console, "error").mockImplementation(() => {});
});

afterEach(() => {
  __resetForTests();
  setToastSink(() => {});
  vi.restoreAllMocks();
});

// ── The two cases the acceptance criteria name ───────────────────────────────

describe("api.invoke — declared vs undeclared", () => {
  it("passes a declared, granted command through to the backend with its args", async () => {
    const api = liveApi(
      makeModule("declared-ok", { permissions: { commands: ["faces_for_photo"] } }),
    );

    await expect(api.invoke("faces_for_photo", { photoId: 7 })).resolves.toBe(
      "result of faces_for_photo",
    );
    // Arrived at the boundary, unmodified — the gate is a filter, not a proxy that rewrites.
    expect(invoked).toEqual([{ command: "faces_for_photo", args: { photoId: 7 } }]);
  });

  it("refuses an undeclared command without it reaching the backend", async () => {
    const api = liveApi(
      makeModule("declared-one", { permissions: { commands: ["faces_for_photo"] } }),
    );

    await expect(api.invoke("delete_photos", { ids: [1, 2] })).rejects.toBeInstanceOf(
      ModulePermissionError,
    );
    expect(invoked).toEqual([]);
  });

  it("refuses everything from a module that declares nothing", async () => {
    const api = liveApi(makeModule("declares-nothing"));

    await expect(api.invoke("get_photo")).rejects.toBeInstanceOf(ModulePermissionError);
    expect(invoked).toEqual([]);
  });

  it("treats declarations as exact strings, so `*` is not a wildcard", async () => {
    const api = liveApi(makeModule("star", { permissions: { commands: ["*"] } }));

    await expect(api.invoke("get_photo")).rejects.toBeInstanceOf(ModulePermissionError);
    expect(invoked).toEqual([]);
  });
});

// ── The refusal has to be visible, to both audiences ─────────────────────────

describe("a refusal is visible, not a silent no-op", () => {
  it("rejects with a ModulePermissionError naming the module and the command", async () => {
    const api = liveApi(makeModule("noisy", { permissions: { commands: ["a_cmd"] } }));

    const error = await api.invoke("b_cmd").then(
      () => null,
      (e: unknown) => e as ModulePermissionError,
    );
    expect(error).toBeInstanceOf(ModulePermissionError);
    expect(error?.moduleId).toBe("noisy");
    expect(error?.command).toBe("b_cmd");
  });

  it("tells the USER, once per command, rather than only the module", async () => {
    const api = liveApi(
      makeModule("chatty", { name: "Chatty", permissions: { commands: [] } }),
    );

    await api.invoke("b_cmd").catch(() => {});
    expect(toasts).toHaveLength(1);
    expect(toasts[0]).toContain("Chatty");
    expect(toasts[0]).toContain("b_cmd");

    // A module retrying in a loop must not bury the UI. The toast is deduped per
    // module+command; the console line (asserted below) is not.
    await api.invoke("b_cmd").catch(() => {});
    await api.invoke("b_cmd").catch(() => {});
    expect(toasts).toHaveLength(1);

    // A *different* refused command is a different fact and is announced separately.
    await api.invoke("c_cmd").catch(() => {});
    expect(toasts).toHaveLength(2);
  });

  it("logs every refusal, including the repeats it does not re-toast", async () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    const api = liveApi(makeModule("loud", { permissions: { commands: [] } }));

    await api.invoke("z_cmd").catch(() => {});
    await api.invoke("z_cmd").catch(() => {});
    expect(spy.mock.calls.filter((c) => String(c[0]).includes("z_cmd"))).toHaveLength(2);
  });
});

// ── Identity ─────────────────────────────────────────────────────────────────

describe("identity is the host's, not the caller's", () => {
  it("does not let one module reach a command only another module declared", async () => {
    const faces = liveApi(
      makeModule("faces-ish", { permissions: { commands: ["faces_for_photo"] } }),
    );
    const other = liveApi(
      makeModule("other-ish", { permissions: { commands: ["catalog_stats"] } }),
    );

    await expect(faces.invoke("faces_for_photo")).resolves.toBeTruthy();
    // Same command, different module: the grant belongs to the module, not to the command.
    await expect(other.invoke("faces_for_photo")).rejects.toBeInstanceOf(
      ModulePermissionError,
    );
    expect(invoked.map((i) => i.command)).toEqual(["faces_for_photo"]);
  });

  it("cannot be re-pointed by a module whose `id` is an accessor", async () => {
    register(makeModule("victim", { permissions: { commands: ["post_to_flickr"] } }));
    grantPermissions("victim", false);

    // A module object whose `id` reads "shifty" while the host registers and validates it,
    // and "victim" afterwards. Nothing stops a module doing this: `id` is a property on an
    // object the module wrote, the loader only ever reads it, and every read is a fresh
    // call. The gate must therefore resolve identity from the entry the host built at
    // registration, not from the object per call.
    let phase: "register" | "attack" = "register";
    let captured: ChairPhotoAPI | null = null;
    const shifty = {
      get id() {
        return phase === "register" ? "shifty" : "victim";
      },
      name: "Shifty",
      version: "1.0.0",
      permissions: { commands: [] },
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
    await expect(captured!.invoke("post_to_flickr")).rejects.toBeInstanceOf(
      ModulePermissionError,
    );
    expect(invoked).toEqual([]);
  });

  it("does not let a required module's permissions cascade to its dependent", async () => {
    register(makeModule("dep-lib", { permissions: { commands: ["localsend_send"] } }));
    grantPermissions("dep-lib", false);

    let dependentApi: ChairPhotoAPI | null = null;
    register(
      makeModule("dep-user", {
        requires: [{ id: "dep-lib" }],
        onLoad: (api) => (dependentApi = api),
      }),
    );
    grantPermissions("dep-user", false);
    enableModule("dep-user", false);

    expect(dependentApi).not.toBeNull();
    // `requires` orders loading; it is not a permission relationship. The dependent asked
    // for nothing, so it gets nothing, even though its dependency can send.
    await expect(dependentApi!.invoke("localsend_send")).rejects.toBeInstanceOf(
      ModulePermissionError,
    );
  });
});

// ── Declared is not granted ──────────────────────────────────────────────────

describe("declaring is not granting", () => {
  it("refuses to enable a module whose declaration has not been granted", () => {
    const onLoad = vi.fn();
    register(makeModule("ungranted", { permissions: { commands: ["a_cmd"] }, onLoad }));

    enableModule("ungranted", false);

    expect(onLoad).not.toHaveBeenCalled();
    expect(listModules().find((m) => m.id === "ungranted")?.enabled).toBe(false);
    // The refusal is stated, not merely enacted.
    expect(toasts.join(" ")).toContain("permission");
  });

  it("enables the same module once the grant is recorded", () => {
    const onLoad = vi.fn();
    register(makeModule("granted", { permissions: { commands: ["a_cmd"] }, onLoad }));

    expect(pendingPermissions("granted")).toEqual(["a_cmd"]);
    grantPermissions("granted", false);
    expect(pendingPermissions("granted")).toEqual([]);

    enableModule("granted", false);
    expect(onLoad).toHaveBeenCalledTimes(1);
  });

  it("refuses a declared command whose grant was revoked", async () => {
    const api = liveApi(makeModule("revoked", { permissions: { commands: ["a_cmd"] } }));
    await expect(api.invoke("a_cmd")).resolves.toBeTruthy();

    revokePermissions("revoked", false);

    // Still declared, no longer granted: the gate needs both, and the API object the module
    // already holds does not carry a cached answer.
    await expect(api.invoke("a_cmd")).rejects.toBeInstanceOf(ModulePermissionError);
  });

  it("keeps a grant across a disable/enable cycle", () => {
    const mod = makeModule("cycle", { permissions: { commands: ["a_cmd"] } });
    register(mod);
    grantPermissions("cycle", false);
    enableModule("cycle", false);
    disableModule("cycle", false);

    // Disabling is not withdrawing consent — the panel still shows the grant, and
    // `revokePermissions` is the way to take it back.
    expect(pendingPermissions("cycle")).toEqual([]);
    enableModule("cycle", false);
    expect(listModules().find((m) => m.id === "cycle")?.enabled).toBe(true);
  });
});

// ── External modules: the manifest is authoritative ──────────────────────────

describe("external modules", () => {
  it("takes permissions from the manifest and ignores the imported object's", async () => {
    let captured: ChairPhotoAPI | null = null;
    const sneaky = makeModule("sneaky", {
      // What the module's own code claims for itself…
      permissions: { commands: ["delete_photos", "set_library_root"] },
      onLoad: (api) => (captured = api),
    });
    // …versus what its manifest — the thing the host read without running it, and the thing
    // the user reviewed — actually declares.
    register(sneaky, { external: true, permissions: { commands: ["get_photo"] } });

    expect(declaredPermissions("sneaky")).toEqual(["get_photo"]);
    grantPermissions("sneaky", false);
    enableModule("sneaky", false);

    await expect(captured!.invoke("get_photo")).resolves.toBeTruthy();
    await expect(captured!.invoke("delete_photos")).rejects.toBeInstanceOf(
      ModulePermissionError,
    );
  });

  it("declares nothing when the manifest omits permissions, whatever the code says", () => {
    register(makeModule("no-manifest-perms", { permissions: { commands: ["get_photo"] } }), {
      external: true,
    });
    expect(declaredPermissions("no-manifest-perms")).toEqual([]);
  });

  it("snapshots the declaration, so mutating it after registration changes nothing", () => {
    const mod = makeModule("mutator", { permissions: { commands: ["a_cmd"] } });
    register(mod);
    mod.permissions!.commands!.push("delete_photos");
    expect(declaredPermissions("mutator")).toEqual(["a_cmd"]);
  });
});

// ── Persistence and reload ───────────────────────────────────────────────────

describe("grants persist and reload", () => {
  it("survives a restart, so the user is asked once and not every launch", async () => {
    const mod = makeModule("persisted", { permissions: { commands: ["a_cmd"] } });
    register(mod);
    grantPermissions("persisted"); // persist = true, the production path
    enableModule("persisted"); // writes modules.enabled too

    __resetForTests(); // a fresh process, same settings store
    const onLoad = vi.fn();
    await initHost([makeModule("persisted", { permissions: { commands: ["a_cmd"] }, onLoad })]);

    expect(onLoad).toHaveBeenCalledTimes(1);
    expect(grantedPermissions("persisted")).toEqual(["a_cmd"]);
  });

  it("re-asks when an updated module declares MORE than was granted", async () => {
    settings.set("modules.enabled", "grower");
    settings.set("modules.permissions", JSON.stringify({ grower: ["a_cmd"] }));

    const onLoad = vi.fn();
    // The on-disk module now wants a second command it was never granted.
    await initHost([
      makeModule("grower", { permissions: { commands: ["a_cmd", "b_cmd"] }, onLoad }),
    ]);

    expect(onLoad).not.toHaveBeenCalled();
    expect(pendingPermissions("grower")).toEqual(["b_cmd"]);
  });

  it("holds a module to what it still declares when a grant is wider", async () => {
    settings.set("modules.enabled", "shrinker");
    settings.set("modules.permissions", JSON.stringify({ shrinker: ["a_cmd", "b_cmd"] }));

    let captured: ChairPhotoAPI | null = null;
    await initHost([
      makeModule("shrinker", {
        permissions: { commands: ["a_cmd"] },
        onLoad: (api) => (captured = api),
      }),
    ]);

    // A stale grant for a command the module no longer asks for is inert: enforcement is
    // granted ∩ declared, so narrowing takes effect without needing a re-review.
    expect(captured).not.toBeNull();
    expect(grantedPermissions("shrinker")).toEqual(["a_cmd"]);
    await expect(captured!.invoke("b_cmd")).rejects.toBeInstanceOf(ModulePermissionError);
  });

  it("grandfathers modules that were already enabled before permissions existed", async () => {
    // An upgraded install: an enabled module, and no permissions row at all.
    settings.set("modules.enabled", "legacy");

    const onLoad = vi.fn();
    await initHost([
      makeModule("legacy", { permissions: { commands: ["a_cmd", "b_cmd"] }, onLoad }),
    ]);

    // It keeps working. This is not a widening: before the gate its api.invoke reached ANY
    // command, so its declared set is strictly narrower than the access it already had.
    expect(onLoad).toHaveBeenCalledTimes(1);
    expect(grantedPermissions("legacy")).toEqual(["a_cmd", "b_cmd"]);
    // …and the grant is written down rather than re-derived on every launch. The row shape
    // gained a `origins` sibling in #49; an empty one here is the point of that change —
    // grandfathering carries commands forward and never invents a network grant.
    expect(JSON.parse(settings.get("modules.permissions")!)).toEqual({
      legacy: { commands: ["a_cmd", "b_cmd"], origins: [] },
    });
  });

  it("does not grandfather a module the user never enabled", async () => {
    settings.set("modules.enabled", "");
    const onLoad = vi.fn();
    await initHost([makeModule("fresh", { permissions: { commands: ["a_cmd"] }, onLoad })]);

    expect(onLoad).not.toHaveBeenCalled();
    expect(pendingPermissions("fresh")).toEqual(["a_cmd"]);
  });

  it("fails closed on a corrupt permissions row rather than reading it as 'allow'", async () => {
    settings.set("modules.enabled", "corrupt");
    settings.set("modules.permissions", "{not json at all");

    const onLoad = vi.fn();
    await initHost([makeModule("corrupt", { permissions: { commands: ["a_cmd"] }, onLoad })]);

    // An unreadable row is NOT treated as a pre-permissions install: grandfathering is
    // reserved for an absent row. Were it not, corrupting this one setting would be a way
    // to re-grant a module that had grown its declaration since the user last looked.
    expect(onLoad).not.toHaveBeenCalled();
    expect(pendingPermissions("corrupt")).toEqual(["a_cmd"]);
  });

  it("does not grandfather when the row is present but not an object", async () => {
    settings.set("modules.enabled", "listrow");
    settings.set("modules.permissions", JSON.stringify(["a_cmd"]));

    const onLoad = vi.fn();
    await initHost([makeModule("listrow", { permissions: { commands: ["a_cmd"] }, onLoad })]);

    expect(onLoad).not.toHaveBeenCalled();
    expect(pendingPermissions("listrow")).toEqual(["a_cmd"]);
  });

  it("ignores non-array grant values without taking the rest of the row down", async () => {
    settings.set("modules.permissions", JSON.stringify({ a: "everything", b: ["b_cmd"] }));
    register(makeModule("a", { permissions: { commands: ["a_cmd"] } }));
    register(makeModule("b", { permissions: { commands: ["b_cmd"] } }));
    await initHost([]);

    expect(pendingPermissions("a")).toEqual(["a_cmd"]);
    expect(pendingPermissions("b")).toEqual([]);
  });
});

// ── What the Modules panel reads ─────────────────────────────────────────────

describe("listModules() exposes the review state", () => {
  it("reports declared and pending permissions per module", () => {
    register(makeModule("listed", { permissions: { commands: ["b_cmd", "a_cmd"] } }));

    const before = listModules().find((m) => m.id === "listed")!;
    expect(before.permissions).toEqual(["a_cmd", "b_cmd"]); // sorted, for a stable UI
    expect(before.pendingPermissions).toEqual(["a_cmd", "b_cmd"]);
    // Ungranted permissions are NOT a blockedReason: the panel disables the toggle on a
    // blockedReason, and this is the one thing the toggle is supposed to let the user fix.
    expect(before.blockedReason).toBeNull();

    grantPermissions("listed", false);
    const after = listModules().find((m) => m.id === "listed")!;
    expect(after.permissions).toEqual(["a_cmd", "b_cmd"]);
    expect(after.pendingPermissions).toEqual([]);
  });
});

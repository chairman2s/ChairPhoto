// The plugin host runtime: registers bundled modules, enables/disables them
// (persisted), injects a per-module host API, and collects their UI contributions.
// The app reads contributions via the slot getters and the useHost() hook.
// See docs/plugin-system.md.

import { useSyncExternalStore } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  appVersion,
  assetUrl,
  assignTag,
  deletePublication,
  getEditRecord,
  getSetting,
  listExternalModules,
  listPublications,
  listTags,
  moduleFetch,
  pluginFeatures,
  recordPublication,
  setEditRecord,
  setSetting,
} from "./api";
import type {
  ChairPhotoAPI,
  ChairPhotoModule,
  EditRecord,
  EditRenderer,
  MainView,
  ModuleFetchInit,
  ModuleFetchResponse,
  ModulePanel,
  ModulePermissions,
  Photo,
  PublishTarget,
  SettingsPanel,
  ToolbarAction,
  Unsubscribe,
} from "./registry";

interface Registered {
  module: ChairPhotoModule;
  /**
   * The module's id, read ONCE at registration and never again.
   *
   * `module.id` is a property on an object a third party wrote, so it can be an accessor
   * that returns one value while the loader is validating it against the manifest and
   * another afterwards. Everything that decides what a module may do — the permission
   * lookup, the settings namespace, the publication marker, the grant the Modules panel
   * records — keys on THIS string, so a module cannot answer "who are you?" differently
   * depending on when it is asked.
   */
  id: string;
  enabled: boolean;
  /** True if loaded from disk (external), false for a bundled module. */
  external: boolean;
  /** Lowest host version this module supports (external only; "" = unconstrained). */
  minHostVersion: string;
  /** The backend commands this module DECLARED, snapshotted at registration. Declaring is
   *  not granting: `permits()` also requires the user's grant. See the permissions section
   *  below. */
  declaredCommands: Set<string>;
  /** The network origins this module DECLARED for `api.fetch`, normalised and snapshotted at
   *  registration, same rules as `declaredCommands` (#49). */
  declaredOrigins: Set<string>;
  panels: ModulePanel[];
  actions: ToolbarAction[];
  /** A React thunk (bundled) or a `ModuleMount` (external); Preferences renders either
   *  through `<ModuleSettings>`. */
  settings: SettingsPanel[];
  mainViews: MainView[];
  publishTargets: PublishTarget[];
  renderer: EditRenderer | null;
}

const modules = new Map<string, Registered>();
let features: string[] = [];
let selection: Photo[] = [];
let activeId: number | null = null;
let activeVersionId: number | null = null;
// The tag open in the tag editor modal (App → host sink), for "tag-editor" panels.
let editingTag: import("./registry").Tag | null = null;
let toast: (msg: string) => void = (m) => console.log("[toast]", m);
let changeSink: () => void = () => {};
// Navigation the host (App) wires: a module can ask to return to the Library and
// filter by a tag or select a photo (used by the Tag Graph module); or select a
// photo silently (without switching the active view, used by the Map filmstrip).
let navSink: {
  filterByTag: (tagId: number) => void;
  selectPhoto: (photoId: number) => void;
  selectPhotoSilent: (photoId: number) => void;
} = {
  filterByTag: () => {},
  selectPhoto: () => {},
  selectPhotoSilent: () => {},
};
let filterContext: { tagId: number | null; albumId: number | null; batchId: number | null } = {
  tagId: null,
  albumId: null,
  batchId: null,
};

// --- React subscription ---------------------------------------------------
//
// One version-counter "channel" per concern a consumer might care about, so a change to
// one (e.g. the photo selection) does not force a re-render in a consumer that only reads
// another (e.g. the Modules panel, which reads only lifecycle). Each channel is a small,
// independent pub/sub — pure and unit-testable without React via `__channels` below (see
// src/modules/__tests__/host.test.ts; `useSyncExternalStore` can't be driven outside a
// component, which is why the pure store and the hook are kept separate). `useHost()` (no
// selector) is the union of every channel, for consumers that legitimately need to react to
// any host change (e.g. App).
const legacyListeners = new Set<() => void>();
let legacyVersion = 0;
/** Bump the legacy "any change" version and run every legacy listener. Each listener runs
 *  through `callSafely` (defined below the safe-dispatch section; hoisted, so the forward
 *  reference is fine) so one throwing listener — e.g. a broken `useHost()` consumer — can't
 *  stop the rest from being notified. */
function bumpLegacy() {
  legacyVersion++;
  legacyListeners.forEach((l) => callSafely("[host] legacy listener threw:", l));
}
function subscribeLegacy(l: () => void) {
  legacyListeners.add(l);
  return () => {
    legacyListeners.delete(l);
  };
}

function createChannel() {
  const listeners = new Set<() => void>();
  let version = 0;
  return {
    subscribe(l: () => void) {
      listeners.add(l);
      return () => {
        listeners.delete(l);
      };
    },
    getVersion: () => version,
    /** Test-only: drop every subscriber without touching `version`. A test that subscribes
     *  and then fails before its unsubscribe would otherwise leave a live listener that
     *  later tests' `notify()` calls still invoke — the same leak `__resetForTests` exists
     *  to stop, one layer down. */
    clearListeners() {
      listeners.clear();
    },
    /** Bump this channel's own subscribers, and the legacy "any change" channel
     *  `useHost()` reads, so a call site never has to remember to notify both. Each
     *  channel listener runs through `callSafely` so a throw can't make `bumpLegacy()`
     *  unreachable (issue #16 Finding 4) — the legacy bump is the compatibility guarantee
     *  every pre-#16 `useHost()` consumer still relies on. */
    notify() {
      version++;
      listeners.forEach((l) => callSafely("[host] channel listener threw:", l));
      bumpLegacy();
    },
  };
}

/** Contributions a module registers: panels, actions, main views, publish targets, and
 *  the edit renderer. Consumers: PhotoInspector (inspector panels), TagEditor (tag-editor
 *  panels), PublishDialog (publish targets), App (main views / toolbar actions). Also
 *  bumped by `enableModule`/`disableModule` (see `notifyModuleSetChanged`), since every
 *  getter here filters by `.enabled`. */
const contributionsChannel = createChannel();
/** The photo selection / active photo / active version (`setSelection`,
 *  `setActiveVersion`). Consumers: module panels that read `api.getActivePhotoId()` /
 *  `getActiveVersionId()` (AI tagging, Faces, Obsidian, Smart Tagging). */
const selectionChannel = createChannel();
/** The sidebar filter context (`setFilterContext`), read via `api.getFilterContext()`. */
const filterContextChannel = createChannel();
/** Settings panels a module registers (`registerSettingsPanel`). Kept separate from
 *  `contributionsChannel` because Preferences needs exactly this slice, not panel/action/
 *  main-view churn from modules it isn't showing a settings tab for. Also bumped by
 *  `enableModule`/`disableModule`. */
const settingsPanelsChannel = createChannel();
/** A module's enabled/disabled state and the module registry itself (`register`,
 *  `enableModule`, `disableModule`, `initHost`). Consumers: ModulesPanel, Preferences. */
const lifecycleChannel = createChannel();
/** The tag open in the tag-editor modal (`setEditingTagContext`), read via
 *  `api.getEditingTag()`. Consumer: the Obsidian module's tag-note panel. */
const editingTagChannel = createChannel();

/** Enabling/disabling a module changes not just its own `enabled` flag but everything
 *  every contribution getter returns, because each one filters by `.enabled`
 *  (`panelsForSlot`, `settingsPanels`/`settingsPanelsForModule`, `publishTargets`,
 *  `mainViews`, `toolbarActions`, `activeEditRenderer`). So a lifecycle change must notify
 *  all three channels those getters feed, not lifecycle alone — otherwise, e.g., disabling
 *  a module would silently stop notifying a consumer subscribed only to contributions. */
function notifyModuleSetChanged() {
  lifecycleChannel.notify();
  contributionsChannel.notify();
  settingsPanelsChannel.notify();
}

/** Re-renders the caller on ANY host change (module lifecycle, contributions, selection,
 *  filter context, settings panels, or the editing-tag context). Prefer a narrower
 *  `useHostX()` selector below when the consumer only cares about one slice — e.g. a
 *  selection change should not invalidate a settings-only consumer like Preferences. */
export function useHost(): number {
  return useSyncExternalStore(subscribeLegacy, () => legacyVersion);
}

/** Re-renders on module contribution changes (panels/actions/main views/publish
 *  targets/edit renderer) and on module enable/disable (which changes what those getters
 *  return). Does NOT re-render on selection, filter context, or settings-panel-only
 *  changes. */
export function useHostContributions(): number {
  return useSyncExternalStore(contributionsChannel.subscribe, contributionsChannel.getVersion);
}

/** Re-renders on photo selection / active photo / active version changes only. */
export function useHostSelection(): number {
  return useSyncExternalStore(selectionChannel.subscribe, selectionChannel.getVersion);
}

/** Re-renders when the sidebar filter context changes only. */
export function useHostFilterContext(): number {
  return useSyncExternalStore(filterContextChannel.subscribe, filterContextChannel.getVersion);
}

/** Re-renders on settings-panel contributions and module enable/disable (which changes
 *  which modules' settings panels are live). Does NOT re-render on other contribution
 *  types (inspector panels, actions, …) or on selection. */
export function useHostSettingsPanels(): number {
  return useSyncExternalStore(settingsPanelsChannel.subscribe, settingsPanelsChannel.getVersion);
}

/** Re-renders on module enable/disable and module-registry changes only (not on the
 *  contributions those modules register, nor on selection). */
export function useHostLifecycle(): number {
  return useSyncExternalStore(lifecycleChannel.subscribe, lifecycleChannel.getVersion);
}

/** Re-renders when the tag open in the tag-editor modal changes only. */
export function useHostEditingTag(): number {
  return useSyncExternalStore(editingTagChannel.subscribe, editingTagChannel.getVersion);
}

/** Test-only access to the per-channel pub/sub, so subscription/notify semantics — and the
 *  isolation the safe dispatcher below provides — can be unit-tested without a React render.
 *  Mirrors the `__events` convention in `src/__test_stubs__/tauri-api-event.ts`. Not part of the host
 *  API surface modules use. */
export const __channels = {
  contributions: contributionsChannel,
  selection: selectionChannel,
  filterContext: filterContextChannel,
  settingsPanels: settingsPanelsChannel,
  lifecycle: lifecycleChannel,
  editingTag: editingTagChannel,
};

/** Test-only access to the legacy "any change" bus `useHost()` reads, so its
 *  backward-compatibility guarantee — every channel notify also bumps this — can be
 *  asserted directly instead of taken on faith. */
export const __legacy = {
  subscribe: subscribeLegacy,
  getVersion: () => legacyVersion,
};

/** Test-only: return every piece of this module's file-scoped state to its initial value.
 *
 *  The registry and the host state around it are module-level mutable singletons, so a test
 *  that fails partway through leaves whatever it registered behind, and every later test that
 *  iterates enabled modules — anything calling `setSelection`, for one — fails too. One real
 *  failure then reads as a cascade, and the first failure in the list is the only true one
 *  with nothing in the output saying so (issue #54).
 *
 *  Call from a global `afterEach`, never from a test body: the point is that it runs on the
 *  failure path, which a last-line call in an `it` does not.
 *
 *  Modules are dropped rather than disabled through `disableModule`, so teardown cannot itself
 *  throw or depend on a module's `onUnload` behaving. Everything a module contributes (panels,
 *  actions, settings, main views, publish targets, edit renderer) lives inside its `Registered`
 *  entry, so clearing the map clears the contributions with it — there is no separate
 *  settings-panel or contribution registry to miss. */
export function __resetForTests() {
  modules.clear();
  grants.clear();
  announcedRefusals.clear();
  announcedFetchRefusals.clear();
  features = [];
  hostVersion = "";
  selection = [];
  activeId = null;
  activeVersionId = null;
  editingTag = null;
  filterContext = { tagId: null, albumId: null, batchId: null };
  for (const channel of Object.values(__channels)) channel.clearListeners();
  legacyListeners.clear();
}

// --- Safe callback dispatch ------------------------------------------------
//
// Every module-authored callback the host calls directly (`onLoad`, `onUnload`,
// `onPhotoSelected`) runs through `callSafely` so one module's throw can never abort the
// host operation invoking it: `enableModule` still rolls back cleanly, `disableModule`
// still clears contributions/persists/notifies, and `setSelection` still reaches every
// other enabled module and still updates host state. See AGENTS.md "Background work and
// ownership" — a required terminal step (finishing enable/disable/select) must not be
// skipped because a module misbehaved.
/** Invoke `fn`, catching and logging (never silently swallowing) any throw. Returns true
 *  if `fn` completed without throwing, false if it threw (already logged). */
function callSafely(label: string, fn: () => void): boolean {
  try {
    fn();
    return true;
  } catch (e) {
    console.error(label, e);
    return false;
  }
}

/** Set a toast sink (the app wires its own toast here). */
export function setToastSink(fn: (msg: string) => void) {
  toast = fn;
}

/** Set the change sink — called when a module asks the app to refresh. */
export function setChangeSink(fn: () => void) {
  changeSink = fn;
}

/** Set the navigation sink (App wires Library-filter / photo-select / silent-select). */
export function setNavSink(fn: typeof navSink) {
  navSink = fn;
}

/** Tell the host which tag the tag editor modal is showing (null = closed), so
 *  "tag-editor" slot panels can read it via getEditingTag(). */
export function setEditingTagContext(tag: import("./registry").Tag | null) {
  editingTag = tag;
  editingTagChannel.notify();
}

/** Update the sidebar filter context (App → host sink, same style as setSelection).
 *  Notifies filterContextChannel so modules that read the context (e.g. Statistics)
 *  re-render. */
export function setFilterContext(ctx: {
  tagId: number | null;
  albumId: number | null;
  batchId: number | null;
}) {
  filterContext = ctx;
  filterContextChannel.notify();
}

// --- semver: a tiny in-repo matcher (no new dep) ------------------------
/**
 * Does module version `version` satisfy the dependency `range`? Covers what simple
 * `MAJOR.MINOR.PATCH` module versions need:
 * - omitted / "" / "*" / "x" -> any version
 * - exact "X.Y.Z" -> must equal
 * - "^X.Y.Z" -> same major and >= X.Y.Z
 * - ">=X.Y.Z" -> >= X.Y.Z
 * Unparseable inputs are treated conservatively as "does not satisfy" (except the
 * any-wildcards above). Pre-release/build suffixes are ignored.
 */
export function satisfies(version: string, range?: string): boolean {
  const r = (range ?? "").trim();
  if (r === "" || r === "*" || r === "x") return true;

  const parse = (v: string): [number, number, number] | null => {
    const m = /^(\d+)\.(\d+)\.(\d+)/.exec(v.trim());
    return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
  };
  const cmp = (a: [number, number, number], b: [number, number, number]): number =>
    a[0] - b[0] || a[1] - b[1] || a[2] - b[2];

  const ver = parse(version);
  if (!ver) return false;

  if (r.startsWith("^")) {
    const base = parse(r.slice(1));
    if (!base) return false;
    if (cmp(ver, base) < 0) return false;
    // npm caret: ^X.Y.Z (X>0) allows same major; ^0.Y.Z is tighter — same major AND minor
    // (a 0.x bump is treated as breaking), so ^0.1.0 matches 0.1.x but not 0.2.0.
    return base[0] > 0 ? ver[0] === base[0] : ver[0] === 0 && ver[1] === base[1];
  }
  if (r.startsWith(">=")) {
    const base = parse(r.slice(2));
    if (!base) return false;
    return cmp(ver, base) >= 0;
  }
  // bare "X.Y.Z" -> exact match
  const exact = parse(r);
  if (!exact) return false;
  return cmp(ver, exact) === 0;
}

/**
 * Does the running host `host` version satisfy a module's `minHostVersion` floor? The
 * manifest states the *oldest* host it supports; the host is compatible when its own
 * version is >= that floor (the "stable within a major" promise covers the upper bound).
 * An omitted / empty / wildcard floor is always satisfied; an unparseable pair is treated
 * conservatively as unsatisfied. Reuses `satisfies`' parser via a `>=` range.
 */
export function hostSatisfies(host: string, minHostVersion?: string): boolean {
  const m = (minHostVersion ?? "").trim();
  if (m === "" || m === "*" || m === "x") return true;
  return satisfies(host, `>=${m}`);
}

// The running host (app) version, learned once in initHost. "" until known — hostSatisfies
// then treats every non-empty floor as unmet, so a module never loads against an unknown host.
let hostVersion = "";

/**
 * Why module `id` cannot be enabled, or null if it can. Checks the host-version floor
 * (external modules) and each declared requirement: the required module must exist, have its
 * backend feature compiled in, and its `version` must satisfy the requested range. (Does not
 * check whether the dependency is currently enabled -- enableModule auto-enables it.)
 */
export function unmetRequirement(id: string): string | null {
  const reg = modules.get(id);
  if (!reg) return `module "${id}" not found`;
  if (reg.minHostVersion && !hostSatisfies(hostVersion, reg.minHostVersion)) {
    return `requires host ≥ ${reg.minHostVersion}, this is ${hostVersion || "unknown"}`;
  }
  if (!backendAvailable(reg.module)) {
    return `backend "${reg.module.backendFeature}" not included in this build`;
  }
  for (const req of reg.module.requires ?? []) {
    const dep = modules.get(req.id);
    if (!dep) return `requires missing module "${req.id}"`;
    if (!backendAvailable(dep.module)) {
      return `requires "${dep.module.name}" whose backend "${dep.module.backendFeature}" is not in this build`;
    }
    if (!satisfies(dep.module.version, req.version)) {
      return `requires ${dep.module.name} ${req.version ?? "*"} but v${dep.module.version} is installed`;
    }
  }
  return null;
}

// --- Per-module permissions (#48) ----------------------------------------
//
// `api.invoke` used to be a raw command-string pass-through: any enabled module could call
// any Tauri command the app registers, related to it or not. It is now gated twice — a
// command must be DECLARED by the module and GRANTED by the user.
//
// Declared comes from the module: `ChairPhotoModule.permissions` for a bundled module, the
// manifest's `permissions` for an external one (the manifest wins there — see `register`).
// Granted comes from the user, at the moment they enable the module, and is persisted.
// Enforcement is the intersection: `granted ∩ declared`. Intersecting rather than trusting
// the grant alone means a module that later *narrows* its declaration is held to the
// narrower set without needing a re-review, and a stale grant for a command a module no
// longer asks for is inert.
//
// Why enable-time review and not a first-use prompt: see docs/module-capabilities.md
// § "Per-module permissions". In short — at enable time the module has not run yet, the
// decision is the user's to initiate, and refusing is a real option; a first-use prompt
// necessarily interrupts an action the user just asked for, where the only workable answer
// is yes.
//
// This is a boundary for EXTERNAL modules, which reach the backend only through the
// `ChairPhotoAPI` object the host hands them. A bundled module is compiled into the app and
// could import `./api` directly; it declares permissions anyway so that the enforcement
// path is exercised by the modules that actually ship, and so each module's backend surface
// is written down. See docs/plugin-system.md § "Trust model".

/**
 * Grants the user has approved, keyed by module id. **Absence of a key means "never
 * reviewed"**, which is deliberately distinct from a recorded empty grant (a review that
 * granted nothing). Loaded once by `initHost` from the `modules.permissions` setting.
 */
interface Grant {
  commands: Set<string>;
  /** Network destinations for `api.fetch` (#49). Never grandfathered — see `loadGrants`. */
  origins: Set<string>;
}
const grants = new Map<string, Grant>();

/**
 * The settings key holding `grants`.
 *
 * Serialised as `{ "<module id>": { "commands": [...], "origins": [...] } }` since #49. A
 * row written before that is `{ "<module id>": ["<command>", …] }`, and `loadGrants` still
 * reads it as a commands-only grant. The reverse direction is safe by accident and by
 * design: a pre-#49 build reading the new shape sees a non-array, skips the entry, and the
 * module comes up unreviewed — which is the fail-closed answer, not a silent grant.
 */
const GRANTS_KEY = "modules.permissions";

/** Thrown (as a promise rejection) by `api.invoke` when a module asks for a command it has
 *  not declared, or has declared but not been granted. A named class so a module can tell a
 *  refusal apart from a backend error, and so a test can assert on the refusal itself
 *  rather than on message text. */
export class ModulePermissionError extends Error {
  constructor(
    readonly moduleId: string,
    readonly command: string,
  ) {
    super(`module "${moduleId}" is not permitted to invoke "${command}"`);
    this.name = "ModulePermissionError";
  }
}

/** Refusals already toasted, as `"<module id>\u0000<command>"`. A module retrying in a loop
 *  would otherwise bury the UI in identical toasts; the console line is unconditional, so
 *  nothing is lost from the record. */
const announcedRefusals = new Set<string>();

/** Network refusals already toasted, keyed by `JSON.stringify([moduleId, origin])` (#49).
 *  Kept apart from `announcedRefusals` rather than sharing it under a prefix, so no in-band
 *  separator has to be invented for a key with a different arity. */
const announcedFetchRefusals = new Set<string>();

/** Thrown (as a promise rejection) by `api.fetch` when a module asks for a URL whose origin
 *  it has not declared, or has declared but not been granted (#49). A sibling of
 *  `ModulePermissionError` rather than the same class: the two name different things, and a
 *  module handling one should not silently swallow the other. */
export class ModuleNetworkPermissionError extends Error {
  constructor(
    readonly moduleId: string,
    /** The origin of the requested URL, or the raw URL when it could not be parsed. */
    readonly origin: string,
  ) {
    super(`module "${moduleId}" is not permitted to reach "${origin}"`);
    this.name = "ModuleNetworkPermissionError";
  }
}

/** Normalise a declaration into a set of command names, dropping anything unusable. Junk
 *  narrows the set; it can never widen it, so a malformed declaration yields a module that
 *  can invoke less, never one that can invoke more. */
function normalizeCommands(permissions?: ModulePermissions): Set<string> {
  const out = new Set<string>();
  for (const command of permissions?.commands ?? []) {
    if (typeof command === "string" && command !== "") out.add(command);
  }
  return out;
}

/**
 * Canonical form of one declared origin, or null if it is not a usable one.
 *
 * `URL` does the normalising that makes exact matching honest — it lower-cases the scheme
 * and host, punycodes an IDN, and drops the default port — so two spellings of one origin
 * become one string. Everything rejected below is rejected so the declared list means
 * exactly what it says:
 *
 * - **not `https:`** — a cleartext grant is consent to one host that everyone on the network
 *   path also receives. Dropped rather than honoured.
 * - **credentials in the authority** — `https://good.example@evil.example/` reads as
 *   `evil.example` to a parser and as `good.example` to a human skimming a review dialog.
 * - **a path, query or fragment** — the grant is per origin. Accepting `https://h/api/v1`
 *   and then matching only its origin would show the user a scope that is not enforced.
 * - **a host that is not a plain name or IPv4 literal** — `URL` is happy to parse
 *   `https://*.example.com`, which then reads as a wildcard grant in the review dialog while
 *   actually being a grant for a host that cannot exist. There is no wildcard syntax, for the
 *   same reason `commands` has none: a subdomain wildcard covers hosts that do not exist yet
 *   and can be created at will by whoever runs the domain. A trailing dot is refused for the
 *   related reason that `example.com.` and `example.com` are different names that look
 *   identical. IPv6 literals are not declarable either; nothing needs them, and every
 *   character class kept out is one fewer way to write two origins that look like one.
 */
const DECLARABLE_HOST = /^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$/;

function normalizeOrigin(raw: string): string | null {
  if (typeof raw !== "string" || raw.trim() === "") return null;
  let url: URL;
  try {
    url = new URL(raw.trim());
  } catch {
    return null;
  }
  if (url.protocol !== "https:") return null;
  // `URL` has already lower-cased and punycoded the host, so this tests the canonical form.
  if (!DECLARABLE_HOST.test(url.hostname)) return null;
  if (url.username !== "" || url.password !== "") return null;
  if (url.search !== "" || url.hash !== "") return null;
  if (url.pathname !== "" && url.pathname !== "/") return null;
  return url.origin;
}

/** Normalise a declaration into a set of origins, dropping anything unusable — the same
 *  fail-closed direction as `normalizeCommands`. */
function normalizeOrigins(permissions?: ModulePermissions): Set<string> {
  const out = new Set<string>();
  for (const origin of permissions?.origins ?? []) {
    const normalized = normalizeOrigin(origin);
    if (normalized) out.add(normalized);
  }
  return out;
}

/** The commands module `id` declared it needs (sorted; empty if unknown or none). */
export function declaredPermissions(id: string): string[] {
  return [...(modules.get(id)?.declaredCommands ?? [])].sort();
}

/** The commands module `id` has been granted, narrowed to what it still declares (sorted).
 *  This is exactly the set `api.invoke` will accept. */
export function grantedPermissions(id: string): string[] {
  const reg = modules.get(id);
  if (!reg) return [];
  const granted = grants.get(id);
  return [...reg.declaredCommands].filter((c) => granted?.commands.has(c)).sort();
}

/** The commands module `id` declares but has not been granted (sorted). Non-empty means the
 *  user has something to review — either a first enable, or a module that grew its
 *  declaration since it was last reviewed (e.g. after being updated on disk). */
export function pendingPermissions(id: string): string[] {
  const reg = modules.get(id);
  if (!reg) return [];
  const granted = grants.get(id);
  return [...reg.declaredCommands].filter((c) => !granted?.commands.has(c)).sort();
}

/** The network origins module `id` declared for `api.fetch` (sorted, normalised). */
export function declaredOrigins(id: string): string[] {
  return [...(modules.get(id)?.declaredOrigins ?? [])].sort();
}

/** The origins module `id` has been granted, narrowed to what it still declares (sorted).
 *  Exactly the set `api.fetch` will accept. */
export function grantedOrigins(id: string): string[] {
  const reg = modules.get(id);
  if (!reg) return [];
  const granted = grants.get(id);
  return [...reg.declaredOrigins].filter((o) => granted?.origins.has(o)).sort();
}

/** The origins module `id` declares but has not been granted (sorted). Non-empty blocks
 *  enabling, exactly as an ungranted command does. */
export function pendingOrigins(id: string): string[] {
  const reg = modules.get(id);
  if (!reg) return [];
  const granted = grants.get(id);
  return [...reg.declaredOrigins].filter((o) => !granted?.origins.has(o)).sort();
}

/**
 * Record the user's approval of everything module `id` currently declares. Called by the
 * permission-review UI after the user has seen the list — never by `enableModule`, which is
 * the whole point: granting is a decision, not a side effect of toggling.
 *
 * The grant is replaced rather than merged, so a module that narrows its declaration and is
 * re-reviewed sheds the commands it dropped.
 */
export function grantPermissions(id: string, persist = true) {
  const reg = modules.get(id);
  if (!reg) return;
  grants.set(id, {
    commands: new Set(reg.declaredCommands),
    origins: new Set(reg.declaredOrigins),
  });
  if (persist) persistGrants();
  lifecycleChannel.notify();
}

/** Forget module `id`'s grant entirely, so enabling it requires a fresh review. */
export function revokePermissions(id: string, persist = true) {
  if (!grants.delete(id)) return;
  if (persist) persistGrants();
  lifecycleChannel.notify();
}

function persistGrants() {
  const out: Record<string, { commands: string[]; origins: string[] }> = {};
  for (const [id, grant] of grants) {
    out[id] = {
      commands: [...grant.commands].sort(),
      origins: [...grant.origins].sort(),
    };
  }
  void setSetting(GRANTS_KEY, JSON.stringify(out));
}

/**
 * Load persisted grants, reporting whether the row was there at all.
 *
 * The caller needs that distinction, and it is the whole reason this returns anything:
 * **absent** means a pre-permissions install, which `grandfatherEnabledModules` treats as
 * "these modules already had wider access, keep them working". **Present but unreadable**
 * means something else — and must not be quietly upgraded into the same free pass, or a
 * corrupt row would become a way to re-grant a module that had grown its declaration since
 * the user last looked at it. Unreadable therefore means every module is unreviewed.
 *
 * Individual junk values (a grant that is not an array of strings) narrow that one module
 * to "unreviewed" without taking the rest of the row down with them.
 */
async function loadGrants(): Promise<"absent" | "loaded"> {
  grants.clear();
  const raw = (await getSetting(GRANTS_KEY).catch(() => null)) ?? "";
  if (!raw) return "absent";
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    console.error(
      `[modules] could not parse ${GRANTS_KEY}; every module will need permission review:`,
      e,
    );
    return "loaded";
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    console.error(
      `[modules] ${GRANTS_KEY} is not an object; every module will need permission review`,
    );
    return "loaded";
  }
  const strings = (value: unknown): Set<string> =>
    Array.isArray(value)
      ? new Set(value.filter((v): v is string => typeof v === "string"))
      : new Set<string>();
  for (const [id, value] of Object.entries(parsed as Record<string, unknown>)) {
    // A bare array is a pre-#49 row: commands only, and no network grant — `api.fetch` did
    // not exist when it was written, so reading one into an origin grant would be inventing
    // consent the user never gave.
    if (Array.isArray(value)) {
      grants.set(id, { commands: strings(value), origins: new Set() });
      continue;
    }
    if (!value || typeof value !== "object") continue;
    const record = value as Record<string, unknown>;
    grants.set(id, {
      commands: strings(record.commands),
      // Normalised on the way in, so a hand-edited row cannot smuggle in a form the
      // declaration path would have rejected (`http:`, a path, a wildcard host).
      origins: new Set(
        [...strings(record.origins)]
          .map(normalizeOrigin)
          .filter((o): o is string => o !== null),
      ),
    });
  }
  return "loaded";
}

/**
 * One-time compatibility pass for modules the user had already enabled before permissions
 * existed. Their declared set is recorded as granted so they keep working across the
 * upgrade.
 *
 * This is not a new grant: before this gate, an enabled module's `api.invoke` reached *any*
 * command the app registers, so recording exactly what it now declares is strictly narrower
 * than the access it already had. Only modules with no recorded grant at all are touched — a
 * recorded empty grant is a decision the user made, and is left alone.
 *
 * **Network origins are deliberately not grandfathered.** The same argument runs the other
 * way for them: before #49 no module could reach the network through the host at all, so
 * writing a module's declared origins in here would be manufacturing consent to send data
 * off the machine that the user was never asked for — the exact thing the privacy invariant
 * forbids. A grandfathered module that also declares origins therefore still comes up with
 * `pendingOrigins`, is refused by `enableModule`, and goes through review once.
 *
 * A module the user has never enabled is untouched, so a newly installed module still goes
 * through review at its first enable. Called only when the grants row was *absent* — see
 * `loadGrants` for why an unreadable row is not the same thing.
 */
function grandfatherEnabledModules(enabledIds: string[]) {
  let changed = false;
  for (const id of enabledIds) {
    const reg = modules.get(id);
    if (!reg || grants.has(id)) continue;
    grants.set(id, { commands: new Set(reg.declaredCommands), origins: new Set() });
    changed = true;
  }
  if (changed) persistGrants();
}

/**
 * Why module `id` cannot be enabled *for permission reasons*, or null. Kept out of
 * `unmetRequirement` on purpose: an unmet requirement is a dead end the user cannot act on
 * from the Modules panel (it disables the toggle), whereas ungranted permissions are
 * precisely what the toggle is supposed to let them act on.
 */
function permissionBlockReason(id: string): string | null {
  const commands = pendingPermissions(id);
  const origins = pendingOrigins(id);
  if (commands.length === 0 && origins.length === 0) return null;
  const parts: string[] = [];
  if (commands.length > 0) {
    parts.push(`${commands.length} backend command${commands.length === 1 ? "" : "s"}`);
  }
  if (origins.length > 0) {
    parts.push(`${origins.length} network destination${origins.length === 1 ? "" : "s"}`);
  }
  return `needs permission for ${parts.join(" and ")} — review it under Preferences → Modules.`;
}

/** May module `id` invoke `command`? Declared AND granted, both required. */
function permits(id: string, command: string): boolean {
  const reg = modules.get(id);
  if (!reg || !reg.declaredCommands.has(command)) return false;
  return grants.get(id)?.commands.has(command) ?? false;
}

/**
 * May module `id` fetch `rawUrl`, and if so what is the approved origin?
 *
 * Returns the normalised origin and href on success, null on refusal. Both are needed by the
 * caller: the href is what goes to Rust (already WHATWG-canonical, so the backend's parser
 * and this one are looking at the same string), and the origin is what Rust re-derives and
 * compares against, which is how a URL that two parsers read differently gets caught.
 *
 * Same shape as `permits`: declared AND granted, keyed on the id snapshot.
 */
function permitsFetch(id: string, rawUrl: string): { origin: string; href: string } | null {
  let url: URL;
  try {
    url = new URL(rawUrl);
  } catch {
    return null;
  }
  // Checked here as well as in Rust so the refusal a module sees for an `http:` URL is the
  // permission error naming the origin, not a transport error from the backend.
  if (url.protocol !== "https:") return null;
  const reg = modules.get(id);
  if (!reg || !reg.declaredOrigins.has(url.origin)) return null;
  if (!grants.get(id)?.origins.has(url.origin)) return null;
  return { origin: url.origin, href: url.href };
}

/** The origin of `rawUrl` for a refusal message, falling back to the raw string when it does
 *  not parse. Only ever used in an error/toast, never in a decision. */
function originLabel(rawUrl: string): string {
  try {
    return new URL(rawUrl).origin;
  } catch {
    return rawUrl;
  }
}

/**
 * Refuse an `api.invoke`, visibly. The command never reaches the backend; the module sees a
 * rejected promise carrying a `ModulePermissionError`, and the user sees a toast naming the
 * module and the command. Both halves matter — a silent no-op would leave a module looking
 * broken for no stated reason.
 */
function refuseInvoke(id: string, command: string): Promise<never> {
  const error = new ModulePermissionError(id, command);
  const name = modules.get(id)?.module.name ?? id;
  console.error(`[modules] ${error.message}`);
  const key = `${id}\u0000${command}`;
  if (!announcedRefusals.has(key)) {
    announcedRefusals.add(key);
    toast(`${name} tried to use "${command}", which it hasn't asked permission for.`);
  }
  return Promise.reject(error);
}

/**
 * Refuse an `api.fetch`, visibly — the network counterpart of `refuseInvoke`, and deliberately
 * the same shape: nothing is sent, the module gets a rejected promise carrying a
 * `ModuleNetworkPermissionError`, and the user gets one toast per module+origin.
 *
 * The toast says *destination*, not "command", because that is what the user approved and
 * what was overstepped. Deduped per module+origin for the same reason invokes are: a module
 * retrying in a loop must not be able to bury the UI.
 */
function refuseFetch(id: string, rawUrl: string): Promise<never> {
  const origin = originLabel(rawUrl);
  const error = new ModuleNetworkPermissionError(id, origin);
  const name = modules.get(id)?.module.name ?? id;
  console.error(`[modules] ${error.message}`);
  // Its own set, not `announcedRefusals`: telling an invoke key from a fetch key inside one
  // set needs an in-band separator, and the only unambiguous one is a NUL — which, written
  // literally into a source file, compiles, passes tests, and silently breaks grep. Two sets
  // need no separator at all, and JSON keeps the pair unambiguous besides.
  const key = JSON.stringify([id, origin]);
  if (!announcedFetchRefusals.has(key)) {
    announcedFetchRefusals.add(key);
    toast(`${name} tried to reach ${origin}, which it hasn't asked permission for.`);
  }
  return Promise.reject(error);
}

/** Enabled modules that declare a requirement on `id` (direct dependents). */
function dependentsOf(id: string): Registered[] {
  return [...modules.values()].filter(
    (r) => r.enabled && (r.module.requires ?? []).some((req) => req.id === id),
  );
}

// --- API injected into each module --------------------------------------
/**
 * Build the host API for one registered module.
 *
 * Takes the registry ENTRY rather than the module object, and reads identity from
 * `reg.id` — the id captured once at registration. Resolving `mod.id` per call would let a
 * module define `id` as an accessor that returns its own id while the host validates it and
 * another module's afterwards, which is enough to borrow that module's permission grant,
 * settings namespace and publication marker. See `Registered.id`.
 */
function apiFor(reg: Registered): ChairPhotoAPI {
  const id = reg.id;
  return {
    getSelectedPhotos: () => selection,
    getActivePhotoId: () => activeId,
    getActiveVersionId: () => activeVersionId,
    getEditingTag: () => editingTag,
    listTags,
    assignTag,
    // Stamp THIS module's marker (declared publicationMarker, or its id) so a module
    // never handles the platform string — the host enforces the fallback rule.
    recordPublication: (photoId, versionId, url) =>
      recordPublication(photoId, versionId, getPublicationMarker(id), url ?? null).then(
        () => {},
      ),
    listPublications: (photoId) => listPublications(photoId),
    deletePublication: (publicationId) => deletePublication(publicationId),
    // Identity comes from the closure, exactly as `recordPublication` above takes its
    // marker from the module rather than letting it pass a platform string. A module never
    // handles its own id here, so it has nothing to forge: the permission lookup is keyed
    // on `reg.id`, captured when the host registered it.
    invoke: <T,>(command: string, args?: Record<string, unknown>): Promise<T> =>
      permits(id, command) ? invoke<T>(command, args) : refuseInvoke(id, command),
    // The network capability (#49), gated on the same closure identity as `invoke` above and
    // for the same reason: `id` is the registration snapshot, so a module has nothing to
    // forge. Note what is NOT passed to the backend — no module id. The backend could not
    // trust one anyway, so the decision is made here, where identity is known, and Rust is
    // left holding only the invariants that hold for any request (see commands/net.rs).
    fetch: (url: string, init?: ModuleFetchInit): Promise<ModuleFetchResponse> => {
      const permitted = permitsFetch(id, url);
      if (!permitted) return refuseFetch(id, url);
      return moduleFetch(
        // The normalised href, not the module's string: the backend re-parses it and checks
        // that its own reading of the origin is the one approved here, which only means
        // something if both sides are looking at a canonical form.
        permitted.href,
        permitted.origin,
        init?.method ?? "GET",
        init?.headers ?? {},
        init?.body ?? null,
      );
    },
    // Adapter over Tauri's `listen`: unwraps `event.payload` and returns a
    // contract-owned `Unsubscribe`, so no Tauri type reaches the module surface.
    onEvent: <T,>(event: string, handler: (payload: T) => void) =>
      listen<T>(event, (e) => handler(e.payload)).then(
        (unlisten): Unsubscribe =>
          () => {
            unlisten();
          },
      ),
    getSetting: (key) => getSetting(`${id}.${key}`),
    setSetting: (key, value) => setSetting(`${id}.${key}`, value),
    getEditRecord: async (photoId) => {
      const json = await getEditRecord(photoId);
      return json ? (JSON.parse(json) as EditRecord) : null;
    },
    setEditRecord: (photoId, record) =>
      setEditRecord(photoId, JSON.stringify(record)),
    registerEditRenderer: (renderer) => {
      reg.renderer = renderer;
      contributionsChannel.notify();
    },
    registerPanel: (p) => {
      reg.panels.push(p);
      contributionsChannel.notify();
    },
    registerAction: (a) => {
      reg.actions.push(a);
      contributionsChannel.notify();
    },
    registerPublishTarget: (t) => {
      reg.publishTargets.push(t);
      contributionsChannel.notify();
    },
    registerSettingsPanel: (panel) => {
      reg.settings.push(panel);
      settingsPanelsChannel.notify();
    },
    registerMainView: (v) => {
      reg.mainViews.push(v);
      contributionsChannel.notify();
    },
    showToast: (m) => toast(m),
    notifyChange: () => changeSink(),
    filterByTag: (tagId) => navSink.filterByTag(tagId),
    selectPhoto: (photoId) => navSink.selectPhoto(photoId),
    selectPhotoSilent: (photoId) => navSink.selectPhotoSilent(photoId),
    getFilterContext: () => filterContext,
  };
}

// --- lifecycle ----------------------------------------------------------

export function register(
  mod: ChairPhotoModule,
  opts: {
    external?: boolean;
    minHostVersion?: string;
    /** The MANIFEST's permissions, for an external module. Ignored for a bundled one. */
    permissions?: ModulePermissions;
  } = {},
) {
  // Read `mod.id` exactly once. Every later use goes through the snapshot on the entry, so
  // an accessor that changes its answer cannot make the map key and the identity the host
  // enforces on disagree. See `Registered.id`.
  const id = mod.id;
  if (!modules.has(id)) {
    const external = opts.external ?? false;
    modules.set(id, {
      module: mod,
      id,
      enabled: false,
      external,
      minHostVersion: opts.minHostVersion ?? "",
      // For an EXTERNAL module the manifest is authoritative and `mod.permissions` is
      // ignored: the manifest is what the host read without executing the module and what
      // the user reviewed, so a module cannot ship a modest manifest and then widen itself
      // from code. Snapshotting into a Set also means a module mutating its own
      // `permissions` array after registration changes nothing.
      declaredCommands: normalizeCommands(external ? opts.permissions : mod.permissions),
      declaredOrigins: normalizeOrigins(external ? opts.permissions : mod.permissions),
      panels: [],
      actions: [],
      settings: [],
      mainViews: [],
      publishTargets: [],
      renderer: null,
    });
  }
}

/** Load the host: discover compiled features, register bundled modules, load
 *  external (user-installed) modules from disk, and enable the ones the user had
 *  enabled (persisted in settings). */
export async function initHost(bundled: ChairPhotoModule[]) {
  features = await pluginFeatures().catch(() => []);
  hostVersion = await appVersion().catch(() => "");
  bundled.forEach((m) => register(m));
  await loadExternalModules();
  // Grants must be loaded before the enable pass below: `enableModule` refuses a module
  // whose declaration is not fully granted, so restoring the user's enabled set depends on
  // knowing what they already approved.
  const grantsRow = await loadGrants();
  const csv = (await getSetting("modules.enabled").catch(() => null)) ?? "";
  const enabledIds = csv.split(",").filter(Boolean);
  if (grantsRow === "absent") grandfatherEnabledModules(enabledIds);
  for (const id of enabledIds) enableModule(id, false);
  // Each enableModule() call above already fired notifyModuleSetChanged() for the modules
  // it enabled. This covers the remaining case: modules that got register()'d (bundled or
  // external stubs) but were never enabled — the Modules panel still needs to see them.
  lifecycleChannel.notify();
}

/**
 * Discover and register user-installed (external) modules from
 * `<app_data_dir>/modules/*` (H8). For each manifest the backend found (H8b):
 * - refuse it up front if its `minHostVersion` is newer than the running host (it is
 *   still registered so the Modules panel can show *why* it is blocked);
 * - dynamically `import()` its entrypoint via the asset protocol (`convertFileSrc`);
 * - validate the default export is a well-formed `ChairPhotoModule` whose `id` matches
 *   the manifest;
 * - `register()` it as external. Enabling happens later in `initHost`'s settings pass
 *   (in dependency order, reusing the H10 `requires` mechanism).
 *
 * A module that throws while importing, or whose shape is invalid, is skipped with a
 * console error — it never crashes discovery of the others or the app. Called from
 * `initHost` after bundled registration; exported for tests.
 */
export async function loadExternalModules() {
  const manifests = await listExternalModules().catch((e) => {
    console.error("[modules] external-module discovery failed:", e);
    return [];
  });

  for (const manifest of manifests) {
    // A too-new module: register a stub so the panel lists it (blockedReason surfaces the
    // reason), but never import/execute its code against an incompatible host.
    if (!hostSatisfies(hostVersion, manifest.minHostVersion)) {
      registerExternalStub(manifest);
      console.warn(
        `[modules] external module "${manifest.id}" needs host ≥ ${manifest.minHostVersion} ` +
          `(this is ${hostVersion || "unknown"}); not loading its code`,
      );
      continue;
    }

    // Resolve entrypoint (relative to the module dir) to an asset URL and import it.
    // A leading slash / drive on `entrypoint` would let a manifest escape its dir; keep it
    // relative by stripping leading separators before joining.
    const rel = manifest.entrypoint.replace(/^[\\/]+/, "");
    const entryPath = `${manifest.moduleDir}/${rel}`;
    let mod: ChairPhotoModule;
    try {
      const imported = (await import(/* @vite-ignore */ assetUrl(entryPath))) as {
        default?: unknown;
      };
      const shapeError = validateModuleShape(imported.default, manifest);
      if (shapeError) {
        console.error(
          `[modules] external module "${manifest.id}" (${entryPath}): ${shapeError}`,
        );
        continue;
      }
      mod = imported.default as ChairPhotoModule;
    } catch (e) {
      console.error(
        `[modules] external module "${manifest.id}" (${entryPath}) failed to load:`,
        e,
      );
      continue;
    }

    register(mod, {
      external: true,
      minHostVersion: manifest.minHostVersion,
      permissions: manifest.permissions,
    });
  }
}

/**
 * Register a lightweight placeholder for an external module whose code we refused to load
 * (host too old). It carries the manifest identity + requirements so the Modules panel can
 * list it and show its `blockedReason`, but its `onLoad` is a no-op so it can never run.
 */
function registerExternalStub(manifest: import("./api").ExternalModuleManifest) {
  register(
    {
      id: manifest.id,
      name: manifest.name,
      version: manifest.version,
      description: manifest.description,
      backendFeature: manifest.backendFeature,
      requires: manifest.requires,
      onLoad: () => {},
    },
    {
      external: true,
      minHostVersion: manifest.minHostVersion,
      // Carried even though the stub's `onLoad` is a no-op, so the Modules panel can show
      // what the module *would* ask for next to the reason it is blocked.
      permissions: manifest.permissions,
    },
  );
}

/**
 * Validate that `value` is a usable `ChairPhotoModule` and agrees with its manifest.
 * Returns an error string (for logging) or null when the shape is valid. Guards the
 * required members the host calls (`id`, `name`, `version`, `onLoad`) and enforces the
 * doc's rule that a module's runtime `id` must match its manifest `id`.
 */
function validateModuleShape(
  value: unknown,
  manifest: import("./api").ExternalModuleManifest,
): string | null {
  if (!value || typeof value !== "object") {
    return "default export is not a module object";
  }
  const m = value as Partial<ChairPhotoModule>;
  if (typeof m.id !== "string" || m.id === "") return "missing string `id`";
  if (m.id !== manifest.id) {
    return `module id "${m.id}" does not match manifest id "${manifest.id}"`;
  }
  if (typeof m.name !== "string" || m.name === "") return "missing string `name`";
  if (typeof m.version !== "string" || m.version === "") {
    return "missing string `version`";
  }
  if (typeof m.onLoad !== "function") return "missing onLoad()";
  return null;
}

export function backendAvailable(mod: ChairPhotoModule): boolean {
  return !mod.backendFeature || features.includes(mod.backendFeature);
}

/**
 * The publication marker a module stamps on photos it publishes: its declared
 * `publicationMarker`, or the module `id` as the fallback. This is the single rule
 * for "modules must say what photos are marked with" — every publishing path resolves
 * its marker here, so no path can omit one. Unknown ids fall back to the id itself
 * (e.g. the built-in Instagram capability → "instagram").
 */
export function getPublicationMarker(moduleId: string): string {
  return modules.get(moduleId)?.module.publicationMarker ?? moduleId;
}

export function enableModule(id: string, persist = true, seen = new Set<string>()) {
  const reg = modules.get(id);
  if (!reg || reg.enabled) return;
  if (seen.has(id)) return; // dependency cycle: stop recursing
  seen.add(id);

  // Validate THIS module's requirements (existence, backend, version) before touching deps.
  const unmet = unmetRequirement(id);
  if (unmet) {
    toast(`Can't enable ${reg.module.name}: ${unmet}`);
    console.warn(`[modules] refused to enable "${id}": ${unmet}`);
    return;
  }

  // Permissions are granted by review, never by the toggle — that is what makes granting a
  // decision rather than a side effect of enabling. A module whose declaration is not fully
  // granted does not load at all: the grant is all-or-nothing per module, so a module never
  // runs in a half-permitted state nobody wrote or tested for. ModulesPanel routes the user
  // through the review dialog and calls `grantPermissions()` before retrying.
  const permissionBlock = permissionBlockReason(id);
  if (permissionBlock) {
    toast(`Can't enable ${reg.module.name}: it ${permissionBlock}`);
    console.warn(
      `[modules] refused to enable "${id}": ungranted permissions: ` +
        `${pendingPermissions(id).join(", ")}`,
    );
    return;
  }

  // Enable required modules FIRST so onLoad order is deps-before-dependents.
  for (const req of reg.module.requires ?? []) {
    enableModule(req.id, false, seen);
    if (!modules.get(req.id)?.enabled) {
      const why =
        unmetRequirement(req.id) ?? permissionBlockReason(req.id) ?? "could not be enabled";
      toast(`Can't enable ${reg.module.name}: dependency ${req.id} ${why}`);
      console.warn(`[modules] refused to enable "${id}": dependency "${req.id}" ${why}`);
      return;
    }
  }

  reg.enabled = true;
  reg.panels = [];
  reg.actions = [];
  reg.settings = [];
  reg.mainViews = [];
  reg.publishTargets = [];
  reg.renderer = null;
  // onLoad runs untrusted third-party code for external modules (H8a browser-extension
  // trust model). Isolate a throw (via callSafely) so one bad module can't crash the shared
  // enable pass (initHost's settings loop) and take down every module after it. Roll back to
  // a clean disabled state and skip it — the rest of the loop, and notifyModuleSetChanged(),
  // still run.
  const loaded = callSafely(`[modules] "${id}" onLoad() threw; leaving it disabled:`, () =>
    reg.module.onLoad(apiFor(reg)),
  );
  if (!loaded) {
    reg.enabled = false;
    reg.panels = [];
    reg.actions = [];
    reg.settings = [];
    reg.mainViews = [];
    reg.publishTargets = [];
    reg.renderer = null;
    toast(`Can't enable ${reg.module.name}: it failed to load`);
    // Nothing was persisted (the module was never added to the enabled set), but a
    // caller like ModulesPanel's checkbox needs a render to reconcile with the rolled-
    // back `enabled: false` — without this, a controlled checkbox can be left visually
    // checked after a failed enable until some unrelated host change forces a re-render.
    notifyModuleSetChanged();
    return;
  }
  if (persist) persistEnabled();
  notifyModuleSetChanged();
}

export function disableModule(id: string, persist = true) {
  const reg = modules.get(id);
  if (!reg || !reg.enabled) return;

  // Cascade: disable any enabled module that requires this one, so no dependent is
  // left orphaned (depth-first, before we tear this module down).
  for (const dep of dependentsOf(id)) {
    disableModule(dep.id, false);
    toast(`Disabled ${dep.module.name} (it requires ${reg.module.name})`);
  }

  reg.enabled = false;
  // A throwing onUnload must not leave the rest of teardown half-done: contributions must
  // still be cleared, the enabled set still persisted, and subscribers still notified — a
  // module's broken cleanup can't be allowed to leave the module looking enabled on disk
  // (persistEnabled skipped) or the UI un-refreshed (notify skipped). callSafely logs the
  // throw and lets execution continue.
  callSafely(`[modules] "${id}" onUnload() threw:`, () => reg.module.onUnload?.());
  reg.panels = [];
  reg.actions = [];
  reg.settings = [];
  reg.mainViews = [];
  reg.publishTargets = [];
  reg.renderer = null;
  if (persist) persistEnabled();
  notifyModuleSetChanged();
}

function persistEnabled() {
  // Persist in dependency order (deps before dependents) so the next initHost enables
  // them in a valid order even before enableModule's recursion kicks in.
  void setSetting("modules.enabled", enabledIdsInDepOrder().join(","));
}

/** Enabled module ids, topologically sorted so each module follows its requirements. */
function enabledIdsInDepOrder(): string[] {
  const enabled = [...modules.values()].filter((r) => r.enabled).map((r) => r.id);
  const enabledSet = new Set(enabled);
  const out: string[] = [];
  const placed = new Set<string>();
  const visiting = new Set<string>();
  const visit = (id: string) => {
    if (placed.has(id) || !enabledSet.has(id) || visiting.has(id)) return;
    visiting.add(id);
    for (const req of modules.get(id)?.module.requires ?? []) visit(req.id);
    visiting.delete(id);
    placed.add(id);
    out.push(id);
  };
  for (const id of enabled) visit(id);
  return out;
}

/** Notify enabled modules that the photo selection changed. One module's throwing
 *  `onPhotoSelected` must not stop other modules from being notified, nor block the host's
 *  own selection state (and its subscribers) from updating — callSafely isolates each call
 *  so the loop always finishes and the channel is always notified. */
export function setSelection(photos: Photo[], active: number | null) {
  selection = photos;
  activeId = active;
  for (const reg of modules.values()) {
    if (!reg.enabled) continue;
    callSafely(`[modules] "${reg.id}" onPhotoSelected() threw:`, () =>
      reg.module.onPhotoSelected?.(photos, apiFor(reg)),
    );
  }
  selectionChannel.notify(); // let module panels (which read the active photo) re-render
}

/** Tell the host which version is active in the inspector/loupe (null = Original), so
 *  publishing modules can default to it. */
export function setActiveVersion(versionId: number | null) {
  if (activeVersionId === versionId) return;
  activeVersionId = versionId;
  selectionChannel.notify();
}

// --- queries for the app ------------------------------------------------

/** A module's declared requirement, resolved against what's installed, for the UI. */
export interface RequirementInfo {
  id: string;
  /** The required module's display name, or the raw id if it isn't installed. */
  name: string;
  /** The requested semver range (undefined = any). */
  version?: string;
  /** True if the requirement is met (installed, backend present, version satisfies). */
  met: boolean;
}

export interface ModuleInfo {
  id: string;
  name: string;
  version: string;
  description?: string;
  enabled: boolean;
  backendAvailable: boolean;
  backendFeature: string | null;
  /** True if loaded from disk (user-installed), false for a bundled module. */
  external: boolean;
  /** Declared dependencies, resolved for display (empty if none). */
  requires: RequirementInfo[];
  /** Why this module can't be enabled right now, or null if it can. Hard blockers only
   *  (missing dependency, uncompiled backend, host too old) — NOT ungranted permissions,
   *  which the user can resolve from this very panel. */
  blockedReason: string | null;
  /** Every backend command this module declares it needs (sorted; empty if none). Shown
   *  whether or not it is granted, so a grant stays inspectable after the fact. */
  permissions: string[];
  /** The subset of `permissions` not yet granted (sorted). Non-empty means enabling this
   *  module must go through permission review first. */
  pendingPermissions: string[];
  /** Every network origin this module declares for `api.fetch` (sorted, normalised; empty
   *  if none). Shown for the same reason `permissions` is — a destination a module may send
   *  data to should stay readable long after the moment of granting. */
  origins: string[];
  /** The subset of `origins` not yet granted (sorted). */
  pendingOrigins: string[];
}

export function listModules(): ModuleInfo[] {
  return [...modules.values()].map((r) => ({
    // The captured id, not `r.module.id`: the Modules panel feeds this straight back into
    // `grantPermissions(id)` / `enableModule(id)`, so a row that named a module other than
    // the one it describes would record a grant against the wrong module.
    id: r.id,
    name: r.module.name,
    version: r.module.version,
    description: r.module.description,
    enabled: r.enabled,
    backendAvailable: backendAvailable(r.module),
    backendFeature: r.module.backendFeature ?? null,
    external: r.external,
    requires: (r.module.requires ?? []).map((req) => {
      const dep = modules.get(req.id);
      const met =
        !!dep &&
        backendAvailable(dep.module) &&
        satisfies(dep.module.version, req.version);
      return { id: req.id, name: dep?.module.name ?? req.id, version: req.version, met };
    }),
    blockedReason: r.enabled ? null : unmetRequirement(r.id),
    permissions: declaredPermissions(r.id),
    pendingPermissions: pendingPermissions(r.id),
    origins: declaredOrigins(r.id),
    pendingOrigins: pendingOrigins(r.id),
  }));
}

/** The active edit renderer, or null if no enabled module provides one. Consumers
 *  (loupe, export) use it to display/produce edited output, falling back to the
 *  unedited preview when null. The first enabled module with a renderer wins. */
export function activeEditRenderer(): EditRenderer | null {
  for (const reg of modules.values()) {
    if (reg.enabled && reg.renderer) return reg.renderer;
  }
  return null;
}

/** Full-surface views from all enabled modules, for the app's view switcher. */
export function mainViews(): MainView[] {
  const out: MainView[] = [];
  for (const reg of modules.values()) {
    if (reg.enabled) out.push(...reg.mainViews);
  }
  return out;
}

/** Toolbar actions from all enabled modules, shown as buttons in the app topbar. */
export function toolbarActions(): ToolbarAction[] {
  const out: ToolbarAction[] = [];
  for (const reg of modules.values()) {
    if (reg.enabled) out.push(...reg.actions);
  }
  return out;
}

/**
 * Invoke a toolbar action's `onActivate` (the non-modal flavour), injecting the owning
 * module's host API — the same API it got at onLoad. The app uses this for actions without
 * a `render`; modal actions render `action.render(close)` directly. No-op if the action or
 * its module can't be found, or the action has no `onActivate`.
 */
export function activateToolbarAction(actionId: string) {
  for (const reg of modules.values()) {
    if (!reg.enabled) continue;
    const action = reg.actions.find((a) => a.id === actionId);
    if (action) {
      // Outside the issue's original list (onLoad/onUnload/onPhotoSelected) but the same
      // category of untrusted, host-invoked callback — a throw here would otherwise
      // propagate straight into the React event handler that triggered the click.
      callSafely(`[modules] "${reg.id}" toolbar action "${actionId}" onActivate() threw:`, () =>
        action.onActivate?.(apiFor(reg)),
      );
      return;
    }
  }
}

/** Publishing destinations from all enabled modules, for the Publish dialog. */
export function publishTargets(): PublishTarget[] {
  const out: PublishTarget[] = [];
  for (const reg of modules.values()) {
    if (reg.enabled) out.push(...reg.publishTargets);
  }
  return out;
}

export function panelsForSlot(slot: ModulePanel["slot"]): ModulePanel[] {
  const out: ModulePanel[] = [];
  for (const reg of modules.values()) {
    if (reg.enabled) out.push(...reg.panels.filter((p) => p.slot === slot));
  }
  return out;
}

export function settingsPanels(): SettingsPanel[] {
  return [...modules.values()].filter((r) => r.enabled).flatMap((r) => r.settings);
}

/** Settings panels registered by one module (empty if it's disabled or absent). */
export function settingsPanelsForModule(id: string): SettingsPanel[] {
  const reg = modules.get(id);
  return reg && reg.enabled ? reg.settings : [];
}

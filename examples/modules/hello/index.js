// Minimal external ChairPhoto module.
//
// It draws through the framework-agnostic rendering ABI: the host hands `mount(el)` a DOM
// element and the module fills it. Nothing here imports or names React, which is the point
// — an external module is imported straight from disk and has no route to the host's React
// instance, and a host-side React upgrade must not break modules already written.
//
// The pattern below is the whole shape of a stateful external panel:
//   mount(el)          -> build DOM, remember the element
//   unmount(el)        -> forget it (the host empties `el` itself)
//   onPhotoSelected()  -> repaint every element still mounted
//
// onLoad also writes a namespaced setting (stored as `hello.loaded_at`), which is what
// makes the load observable from outside the running app.

/** Elements the host has handed us and not yet taken back. */
const mounted = new Set();

/** Latest selection, as reported by onPhotoSelected. */
let state = { count: 0, activeId: null };

let host = null;

function paint(el) {
  const { count, activeId } = state;

  const line = document.createElement("p");
  line.className = "hello-line";
  line.textContent =
    count === 0
      ? "No photo selected."
      : `${count} photo(s) selected — active id ${activeId ?? "none"}.`;

  const button = document.createElement("button");
  button.type = "button";
  button.className = "hello-greet";
  button.textContent = "Say hello";
  button.addEventListener("click", () => {
    host?.showToast(`Hello from an external module (active photo: ${state.activeId ?? "none"}).`);
  });

  // replaceChildren rather than append: onPhotoSelected repaints an element that already
  // has content, and mount() can run more than once for the same contribution (React
  // StrictMode mounts, unmounts and remounts in development).
  el.replaceChildren(line, button);
}

export default {
  id: "hello",
  name: "Hello",
  version: "1.0.0",
  onLoad(api) {
    host = api;
    api.setSetting("loaded_at", new Date().toISOString());
    api.registerPanel({
      id: "hello-panel",
      label: "Hello",
      slot: "inspector",
      mount(el) {
        mounted.add(el);
        paint(el);
      },
      unmount(el) {
        // Only bookkeeping: the DOM inside `el` is the host's to clear, and the click
        // listener goes with the button it was attached to.
        mounted.delete(el);
      },
    });
  },
  onPhotoSelected(photos, api) {
    host = api;
    state = { count: photos.length, activeId: api.getActivePhotoId() };
    for (const el of mounted) paint(el);
  },
  onUnload() {
    mounted.clear();
    host = null;
  },
};

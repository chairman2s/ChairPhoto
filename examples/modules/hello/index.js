// Minimal external ChairPhoto module.
//
// It renders a bare string rather than elements: an external module is imported
// straight from disk, so it has no way to reach the host's React instance and
// cannot construct elements. `ReactNode` includes strings, so this is the most a
// module can currently contribute to the UI.
//
// onLoad writes a namespaced setting (stored as `hello.loaded_at`), which is what
// makes the load observable from outside the running app.
export default {
  id: "hello",
  name: "Hello",
  version: "1.0.0",
  onLoad(api) {
    api.setSetting("loaded_at", new Date().toISOString());
    api.registerPanel({
      id: "hello-panel",
      label: "Hello",
      slot: "inspector",
      render: () => "Hello from an external module.",
    });
  },
};

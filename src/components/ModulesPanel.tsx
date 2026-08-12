import { useEffect, useState } from "react";
import { getModulesDir } from "../modules/api";
import { disableModule, enableModule, listModules, useHostLifecycle } from "../modules/host";

// Modules section (Preferences → Modules): enable/disable bundled modules and any
// user-installed (external) modules discovered at startup. A module's own settings live
// elsewhere (e.g. the AI tab); this is just the on/off registry.
export function ModulesSection() {
  useHostLifecycle(); // re-render on module enable/disable/registration changes
  const all = listModules();
  const bundled = all.filter((m) => !m.external);
  const external = all.filter((m) => m.external);

  const [modulesDir, setModulesDir] = useState<string>("");
  useEffect(() => {
    getModulesDir()
      .then(setModulesDir)
      .catch(() => setModulesDir(""));
  }, []);

  return (
    <>
      {/* ── Bundled modules ───────────────────────────────────── */}
      <div className="prefs-section">
        <h3>Modules</h3>
        {bundled.length === 0 && (
          <div className="panel-empty">No bundled modules in this build.</div>
        )}
        {bundled.map((m) => (
          <ModuleRow key={m.id} m={m} />
        ))}
      </div>

      {/* ── Installed (external) modules ──────────────────────── */}
      <div className="prefs-section">
        <h3>Installed modules</h3>

        {external.length === 0 && (
          <div className="panel-empty">No external modules installed.</div>
        )}
        {external.map((m) => (
          <ModuleRow key={m.id} m={m} showExternalBadge />
        ))}

        <div className="module-install-hint">
          <div className="modal-sub">
            Drop a module folder into the install directory, then restart to discover it.
            ChairPhoto does not watch for new modules while it is running.
          </div>
          {modulesDir && (
            <div className="module-install-path">
              <span style={{ color: "var(--text-dim)" }}>Install path: </span>
              <code className="module-install-path-code">{modulesDir}</code>
            </div>
          )}
          <div className="modal-sub" style={{ marginTop: 4 }}>
            Each module runs with full app access and can invoke any backend command —
            review the source and verify the author/origin before installing.
          </div>
        </div>
      </div>
    </>
  );
}

// ── Shared row ────────────────────────────────────────────────────────────────

interface ModuleRowProps {
  m: ReturnType<typeof listModules>[number];
  showExternalBadge?: boolean;
}

function ModuleRow({ m, showExternalBadge }: ModuleRowProps) {
  return (
    <div className="module-row">
      <div className="module-info">
        <div className="module-name">
          {m.name}{" "}
          <span className="term-note">v{m.version}</span>
          {showExternalBadge && (
            <span className="module-external-badge">external</span>
          )}
        </div>
        {m.description && <div className="modal-sub">{m.description}</div>}
        {m.requires.length > 0 && (
          <div className="modal-sub">
            Requires:{" "}
            {m.requires.map((req, i) => (
              <span key={req.id} className={req.met ? undefined : "modal-error"}>
                {i > 0 && ", "}
                {req.name} {req.version ?? "*"}
                {!req.met && " (unavailable)"}
              </span>
            ))}
          </div>
        )}
        {!m.backendAvailable && (
          <div className="modal-error">
            backend &ldquo;{m.backendFeature}&rdquo; not included in this build
          </div>
        )}
        {/* Show a host-version or other blockedReason as visible text (not just
            a hover tooltip) so users can see exactly why a module is unavailable
            without hovering.  Only shown when the module is not already enabled
            and the reason isn't already explained by the backend-feature line. */}
        {!m.enabled && m.blockedReason && m.backendAvailable && (
          <div className="modal-error">{m.blockedReason}</div>
        )}
      </div>
      <label
        className="term-export"
        title={!m.enabled && m.blockedReason ? m.blockedReason : undefined}
      >
        <input
          type="checkbox"
          disabled={!m.enabled && !!m.blockedReason}
          checked={m.enabled}
          onChange={(e) => (e.target.checked ? enableModule(m.id) : disableModule(m.id))}
        />
        enabled
      </label>
    </div>
  );
}

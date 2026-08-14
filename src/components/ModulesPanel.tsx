import { useEffect, useState } from "react";
import { getModulesDir } from "../modules/api";
import {
  disableModule,
  enableModule,
  grantPermissions,
  listModules,
  useHostLifecycle,
} from "../modules/host";

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
            A module can only reach the backend commands it declares and you approve, but it
            still runs inside the app with no sandbox — review the source and verify the
            author/origin before installing.
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
  // Open when the user has asked to enable a module that still has ungranted permissions.
  // Enabling is deliberately NOT attempted first: `enableModule` would refuse, and the
  // point of the review is that the grant is a separate decision the user makes with the
  // list in front of them.
  const [reviewing, setReviewing] = useState(false);

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
        {/* What this module can reach through api.invoke, always visible so a grant stays
            inspectable long after it was given — not only in the moment of granting. */}
        {m.permissions.length > 0 && (
          <details className="module-permissions">
            <summary>
              Backend access: {m.permissions.length} command
              {m.permissions.length === 1 ? "" : "s"}
              {m.pendingPermissions.length > 0 && " (needs your approval)"}
            </summary>
            <ul className="module-permission-list">
              {m.permissions.map((c) => (
                <li key={c}>{c}</li>
              ))}
            </ul>
          </details>
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
          onChange={(e) => {
            if (!e.target.checked) {
              disableModule(m.id);
              return;
            }
            // Ungranted permissions are not a blocker the user can only read about — they
            // are the decision this dialog exists to put in front of them.
            if (m.pendingPermissions.length > 0) setReviewing(true);
            else enableModule(m.id);
          }}
        />
        enabled
      </label>

      {reviewing && (
        <PermissionReview
          m={m}
          onClose={() => setReviewing(false)}
          onAllow={() => {
            setReviewing(false);
            grantPermissions(m.id);
            enableModule(m.id);
          }}
        />
      )}
    </div>
  );
}

// ── Permission review ─────────────────────────────────────────────────────────

interface PermissionReviewProps {
  m: ReturnType<typeof listModules>[number];
  onClose: () => void;
  onAllow: () => void;
}

/**
 * The consent step for a module's declared backend permissions (#48).
 *
 * Shown when the user turns a module on, before it has run a single line — which is the
 * whole argument for reviewing here rather than prompting at first use: at this point the
 * user chose the moment, nothing is half-done, and declining costs them nothing but the
 * module. See docs/module-capabilities.md § "Per-module permissions".
 *
 * The grant is all-or-nothing. There is no per-command checkbox, because a module that got
 * three of the five commands it needs would fail in ways nobody wrote or tested for.
 */
function PermissionReview({ m, onClose, onAllow }: PermissionReviewProps) {
  const granted = m.permissions.filter((c) => !m.pendingPermissions.includes(c));
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className="modal module-permission-review"
        role="dialog"
        aria-label={`Permissions for ${m.name}`}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          <div className="modal-headinfo">
            <div className="modal-title">{m.name} wants backend access</div>
            <div className="modal-sub">
              {m.external
                ? "This module was installed from disk. It runs inside ChairPhoto with no sandbox."
                : "This module ships with ChairPhoto."}
            </div>
          </div>
        </div>

        <div className="modal-body">
          <div className="modal-sub">
            It can call the {m.pendingPermissions.length} backend command
            {m.pendingPermissions.length === 1 ? "" : "s"} below, and nothing else. Anything
            it asks for that is not on this list is refused.
          </div>
          <ul className="module-permission-list">
            {m.pendingPermissions.map((c) => (
              <li key={c} className="pending">
                {c}
              </li>
            ))}
          </ul>
          {granted.length > 0 && (
            <div className="modal-sub">
              Already approved: {granted.join(", ")}
            </div>
          )}
          <div className="module-permission-actions">
            <button className="chip" onClick={onClose}>
              Cancel
            </button>
            <button className="chip" onClick={onAllow}>
              Allow and enable
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

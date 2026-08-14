import { useState } from "react";
import { publishTargets, useHostContributions } from "../modules/host";
import { ModuleContent } from "../modules/ModuleContent";

// Unified publish dialog: pick a destination (Instagram / Flickr / SmugMug / …) and fill
// in its site-specific form. Destinations are contributed by enabled publishing modules via
// api.registerPublishTarget — this dialog just lists them and renders the selected one.
export function PublishDialog({ onClose }: { onClose: () => void }) {
  useHostContributions(); // re-render when modules enable/disable or register targets
  const targets = publishTargets();
  const [selected, setSelected] = useState(0);
  const active = targets[Math.min(selected, Math.max(0, targets.length - 1))];

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Publish</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>
        <div className="modal-body">
          {targets.length === 0 ? (
            <div className="panel-empty">
              Enable a publishing module (Instagram, Flickr, SmugMug) in Preferences →
              Modules first.
            </div>
          ) : (
            <>
              <div className="row publish-targets">
                {targets.map((t, i) => (
                  <button
                    key={t.id}
                    className={`chip ${i === selected ? "chip-on" : ""}`}
                    onClick={() => setSelected(i)}
                  >
                    {t.label}
                  </button>
                ))}
              </div>
              <div className="publish-form">
                {active && <ModuleContent view={active} />}
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

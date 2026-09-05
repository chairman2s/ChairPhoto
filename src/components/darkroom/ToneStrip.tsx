// The Darkroom's adjustable histogram (docs/plans/darkroom/mockups/04-tone-strip.html):
// eight EV zones, fill height = pixel mass, and dragging a zone writes an EV offset into
// the edit record's `zones`. The drag math is pure and exported for tests.
import { useId } from "react";

export const ZONE_COUNT = 8;
/** Zone offsets clamp to ±2 EV — beyond that a "zone" nudge stops being a nuance. */
export const MAX_ZONE_EV = 2;
/** Vertical drag pixels per EV. */
const PX_PER_EV = 60;

const ZONE_LABELS = [
  "blacks",
  "deep shadows",
  "shadows",
  "low mids",
  "high mids",
  "lights",
  "highlights",
  "whites",
];
// Dark→light chip ramp so the strip reads as a tonal scale.
const ZONE_FILLS = [
  "#1a1d24",
  "#2a2f3a",
  "#4a5265",
  "#707a90",
  "#9aa3b5",
  "#c3cad8",
  "#e4e8ef",
  "#ffffff",
];

/**
 * One drag step: zone `zone` moved by `deltaEv`, clamped to ±MAX_ZONE_EV. Pure — the
 * input array is never mutated, and a missing/malformed array becomes a zeroed strip.
 */
export function applyZoneDrag(
  zones: number[] | undefined,
  zone: number,
  deltaEv: number,
): number[] {
  const out =
    zones && zones.length === ZONE_COUNT ? [...zones] : (Array(ZONE_COUNT).fill(0) as number[]);
  out[zone] = Math.min(MAX_ZONE_EV, Math.max(-MAX_ZONE_EV, out[zone] + deltaEv));
  return out;
}

export function ToneStrip({
  masses,
  zones,
  onZones,
}: {
  /** Share of pixels per zone (any scale — normalized against the largest). */
  masses: number[];
  /** Current zone EV offsets from the edit record, or undefined = all zero. */
  zones?: number[];
  onZones: (zones: number[]) => void;
}) {
  const id = useId();
  const startDrag = (e: React.PointerEvent, zone: number) => {
    e.preventDefault();
    const startY = e.clientY;
    const startZones = zones;
    const move = (ev: PointerEvent) =>
      onZones(applyZoneDrag(startZones, zone, (startY - ev.clientY) / PX_PER_EV));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };
  const resetZone = (zone: number) => {
    const out = applyZoneDrag(zones, zone, 0);
    out[zone] = 0;
    onZones(out);
  };
  const maxMass = Math.max(...masses, 1e-6);
  return (
    <div className="dk-strip-wrap">
      <div className="dk-strip">
        {ZONE_LABELS.map((label, i) => {
          const dz = zones?.[i] ?? 0;
          return (
            <div
              key={`${id}-${label}`}
              className="dk-zone"
              title={`${label} — drag up/down (±${MAX_ZONE_EV} EV), double-click resets`}
              onPointerDown={(e) => startDrag(e, i)}
              onDoubleClick={() => resetZone(i)}
            >
              {dz !== 0 && (
                <span className="dk-zone-delta">
                  {dz > 0 ? "+" : ""}
                  {dz.toFixed(1)}
                </span>
              )}
              <i
                className="dk-zone-mass"
                style={{
                  height: `${((masses[i] ?? 0) / maxMass) * 100}%`,
                  background: ZONE_FILLS[i],
                }}
              />
            </div>
          );
        })}
      </div>
      <div className="dk-strip-labels">
        {ZONE_LABELS.map((l) => (
          <span key={l}>{l}</span>
        ))}
      </div>
    </div>
  );
}

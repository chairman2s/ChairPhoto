// Canonical colour-label values and their swatch colours — the single source for
// the label vocabulary. Must match what the inspector stores per photo (labels are
// compared case-insensitively in the backend, but use the canonical casing so the
// stored value, smart-album rules, and filters all agree). "" (no label) is the
// column default, not a member of this list.
export const COLOR_LABELS: { name: string; color: string }[] = [
  { name: "Red", color: "#DC2626" },
  { name: "Yellow", color: "#FFD700" },
  { name: "Green", color: "#10B981" },
  { name: "Blue", color: "#3B82F6" },
  { name: "Purple", color: "#8B5CF6" },
];

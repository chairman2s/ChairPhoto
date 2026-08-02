// Collage layout templates for the freeform canvas. A template is a generator: given the
// number of photos it returns one normalized cell rect {x,y,w,h} (0–1 of the canvas) per
// photo, in order. The dialog turns these into placements (cover-cropped, then pannable).
// Cells use the whole canvas (edge-to-edge); add border/corner-radius/gap via the controls.

export interface Cell {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface CollageTemplate {
  id: string;
  label: string;
  /** Cells for `n` photos (length === n), or null if the template doesn't fit this count. */
  gen: (n: number) => Cell[] | null;
}

/** Even rows × cols grid (cols ≈ √n, biased by aspect), row-major, filling all `n`. */
function gridCells(n: number): Cell[] {
  const cols = Math.max(1, Math.round(Math.sqrt(n)));
  const rows = Math.ceil(n / cols);
  const cells: Cell[] = [];
  for (let i = 0; i < n; i++) {
    const r = Math.floor(i / cols);
    const c = i % cols;
    // Last (possibly short) row stretches its cells to fill the width.
    const inRow = r === rows - 1 && n % cols !== 0 ? n % cols : cols;
    cells.push({ x: c / inRow, y: r / rows, w: 1 / inRow, h: 1 / rows });
  }
  return cells;
}

export const COLLAGE_TEMPLATES: CollageTemplate[] = [
  {
    id: "grid",
    label: "Grid",
    gen: (n) => (n >= 1 ? gridCells(n) : null),
  },
  {
    id: "columns",
    label: "Columns",
    gen: (n) => (n >= 1 ? Array.from({ length: n }, (_, i) => ({ x: i / n, y: 0, w: 1 / n, h: 1 })) : null),
  },
  {
    id: "rows",
    label: "Rows",
    gen: (n) => (n >= 1 ? Array.from({ length: n }, (_, i) => ({ x: 0, y: i / n, w: 1, h: 1 / n })) : null),
  },
  {
    id: "feature-left",
    label: "Feature + column (left)",
    gen: (n) => {
      if (n < 2) return null;
      const cells: Cell[] = [{ x: 0, y: 0, w: 0.62, h: 1 }];
      const m = n - 1;
      for (let i = 0; i < m; i++) cells.push({ x: 0.62, y: i / m, w: 0.38, h: 1 / m });
      return cells;
    },
  },
  {
    id: "feature-right",
    label: "Feature + column (right)",
    gen: (n) => {
      if (n < 2) return null;
      const cells: Cell[] = [{ x: 0.38, y: 0, w: 0.62, h: 1 }];
      const m = n - 1;
      for (let i = 0; i < m; i++) cells.push({ x: 0, y: i / m, w: 0.38, h: 1 / m });
      return cells;
    },
  },
  {
    id: "feature-top",
    label: "Feature + strip (top)",
    gen: (n) => {
      if (n < 2) return null;
      const cells: Cell[] = [{ x: 0, y: 0, w: 1, h: 0.62 }];
      const m = n - 1;
      for (let i = 0; i < m; i++) cells.push({ x: i / m, y: 0.62, w: 1 / m, h: 0.38 });
      return cells;
    },
  },
  {
    id: "feature-bottom",
    label: "Feature + strip (bottom)",
    gen: (n) => {
      if (n < 2) return null;
      const cells: Cell[] = [{ x: 0, y: 0.38, w: 1, h: 0.62 }];
      const m = n - 1;
      for (let i = 0; i < m; i++) cells.push({ x: i / m, y: 0, w: 1 / m, h: 0.38 });
      return cells;
    },
  },
];

// In every "feature" template, cells[0] is the large feature slot — so swapping a photo into
// slot 0 (drag-to-swap in locked mode) makes it the feature.

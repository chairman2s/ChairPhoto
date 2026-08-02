// Statistics module: a main-view read-only report on the catalog — timeline, hour/weekday
// shooting patterns, cameras, lenses, focal lengths, ratings and top tags. Frontend-only;
// all figures come from the `catalog_stats` command. See docs/statistics.md.
import { useEffect, useState, useRef, useId } from "react";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import "./statistics.css";

// ── Backend contract ──────────────────────────────────────────────────────────

interface CatalogStats {
  totalPhotos: number;
  withCaptureTime: number;
  firstMonth: string | null;  // "YYYY-MM"
  lastMonth: string | null;
  timeline: [string, number][];   // ["YYYY-MM", count], ascending, gap months ABSENT
  hours: number[];                // 24 ints, index = hour 0-23
  weekdays: number[];             // 7 ints, index 0 = SUNDAY
  topTags: { id: number; label: string; count: number }[];
  cameras: [string, number][];    // desc
  lenses: [string, number][];     // desc
  focalLengths: [number, number][]; // raw focal -> count, ascending
  ratings: number[];              // 6 ints, index = rating 0-5
  topDays: [string, number][];    // 3 busiest single days ["YYYY-MM-DD", count] desc
  invalidDates: number;           // photos with bogus (pre-1950) dates, excluded above
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function parseYM(ym: string): { year: number; month: number } {
  const [y, m] = ym.split("-");
  return { year: parseInt(y, 10), month: parseInt(m, 10) };
}

function fmtMonth(ym: string): string {
  const { year, month } = parseYM(ym);
  const d = new Date(year, month - 1, 1);
  return d.toLocaleString("en-US", { month: "short", year: "numeric" });
}

/** "2015-06-12" → "12 Jun 2015" */
function fmtDay(ymd: string): string {
  const [y, m, d] = ymd.split("-").map((v) => parseInt(v, 10));
  const date = new Date(y, m - 1, d);
  return date.toLocaleString("en-US", { day: "numeric", month: "short", year: "numeric" });
}

function nextMonth(year: number, month: number): { year: number; month: number } {
  return month === 12 ? { year: year + 1, month: 1 } : { year, month: month + 1 };
}

function toYM(year: number, month: number): string {
  return `${year}-${String(month).padStart(2, "0")}`;
}

function buildTimeline(
  sparse: [string, number][],
  first: string,
  last: string,
): [string, number][] {
  const map = new Map(sparse);
  const result: [string, number][] = [];
  let { year, month } = parseYM(first);
  const { year: ey, month: em } = parseYM(last);
  // Guard against a pathological span (should be clamped by backend, but be safe).
  let guard = 0;
  while ((year < ey || (year === ey && month <= em)) && guard < 6000) {
    const ym = toYM(year, month);
    result.push([ym, map.get(ym) ?? 0]);
    ({ year, month } = nextMonth(year, month));
    guard++;
  }
  return result;
}

/** Aggregate monthly [ym,count] to yearly [year,count]. */
function aggregateToYears(monthly: [string, number][]): [string, number][] {
  const map = new Map<string, number>();
  for (const [ym, c] of monthly) {
    const y = ym.slice(0, 4);
    map.set(y, (map.get(y) ?? 0) + c);
  }
  return Array.from(map.entries()).sort((a, b) => a[0].localeCompare(b[0]));
}

/** Focal length bucket boundaries (upper exclusive) */
const FL_BUCKETS: { label: string; max: number }[] = [
  { label: "<16",     max: 16 },
  { label: "16–24",   max: 24 },
  { label: "24–35",   max: 35 },
  { label: "35–50",   max: 50 },
  { label: "50–85",   max: 85 },
  { label: "85–135",  max: 135 },
  { label: "135–200", max: 200 },
  { label: ">200",    max: Infinity },
];

function bucketFocalLengths(raw: [number, number][]): number[] {
  const out = new Array<number>(FL_BUCKETS.length).fill(0);
  for (const [focal, count] of raw) {
    const idx = FL_BUCKETS.findIndex((b) => focal < b.max);
    if (idx !== -1) out[idx] += count;
  }
  return out;
}

function trimZeroBuckets(
  labels: string[],
  values: number[],
): { labels: string[]; values: number[] } {
  let start = 0;
  let end = values.length - 1;
  while (start <= end && values[start] === 0) start++;
  while (end >= start && values[end] === 0) end--;
  if (start > end) return { labels: [], values: [] };
  return {
    labels: labels.slice(start, end + 1),
    values: values.slice(start, end + 1),
  };
}

/** Linear interpolate two hex colours. t ∈ [0,1]. */
function lerpHex(a: string, b: string, t: number): string {
  const pa = [parseInt(a.slice(1, 3), 16), parseInt(a.slice(3, 5), 16), parseInt(a.slice(5, 7), 16)];
  const pb = [parseInt(b.slice(1, 3), 16), parseInt(b.slice(3, 5), 16), parseInt(b.slice(5, 7), 16)];
  const c = pa.map((v, i) => Math.round(v + (pb[i] - v) * t));
  return `#${c.map((v) => v.toString(16).padStart(2, "0")).join("")}`;
}

/** Intensity colour ramp: #334155 (low) → #3B82F6 (accent) → #93C5FD (peak). */
function intensityColor(t: number): string {
  if (t <= 0.5) return lerpHex("#334155", "#3B82F6", t / 0.5);
  return lerpHex("#3B82F6", "#93C5FD", (t - 0.5) / 0.5);
}

// Donut palette (top cameras + Other)
const DONUT_HUES = ["#3B82F6", "#8B5CF6", "#F59E0B", "#EC4899", "#14B8A6", "#64748B"];

// ── Area timeline (the hero) ───────────────────────────────────────────────────

interface AreaProps {
  points: [string, number][];       // [label, value] ascending
  tickEvery: (label: string, i: number) => string; // returns tick text or ""
  fmtValue: (label: string, v: number) => string;   // hover readout
  height?: number;
}

function AreaChart({ points, tickEvery, fmtValue, height = 200 }: AreaProps) {
  const uid = useId().replace(/:/g, "");
  const [hover, setHover] = useState<number | null>(null);
  if (points.length === 0) return <p className="st-empty">No data</p>;

  const W = 1000;
  const H = height;
  const padT = 14;
  const padB = 24;
  const plotH = H - padT - padB;
  const n = points.length;
  const max = Math.max(...points.map((p) => p[1]), 1);

  const x = (i: number) => (n === 1 ? W / 2 : (i / (n - 1)) * W);
  const y = (v: number) => padT + plotH - (v / max) * plotH;

  // Build smooth path (Catmull-Rom → cubic Bézier).
  const pts = points.map((p, i) => [x(i), y(p[1])] as [number, number]);
  let line = `M ${pts[0][0]} ${pts[0][1]}`;
  if (pts.length === 1) {
    line = `M 0 ${pts[0][1]} L ${W} ${pts[0][1]}`;
  } else {
    for (let i = 0; i < pts.length - 1; i++) {
      const p0 = pts[i - 1] ?? pts[i];
      const p1 = pts[i];
      const p2 = pts[i + 1];
      const p3 = pts[i + 2] ?? p2;
      const c1x = p1[0] + (p2[0] - p0[0]) / 6;
      const c1y = p1[1] + (p2[1] - p0[1]) / 6;
      const c2x = p2[0] - (p3[0] - p1[0]) / 6;
      const c2y = p2[1] - (p3[1] - p1[1]) / 6;
      line += ` C ${c1x} ${c1y} ${c2x} ${c2y} ${p2[0]} ${p2[1]}`;
    }
  }
  const baseY = padT + plotH;
  const area = `${line} L ${pts[pts.length - 1][0]} ${baseY} L ${pts[0][0]} ${baseY} Z`;

  // Gridlines at 0, 0.5, 1 of max
  const gridVals = [max, Math.round(max / 2)];

  const onMove = (e: React.PointerEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    const px = ((e.clientX - rect.left) / rect.width) * W;
    let best = 0;
    let bestD = Infinity;
    for (let i = 0; i < n; i++) {
      const d = Math.abs(x(i) - px);
      if (d < bestD) { bestD = d; best = i; }
    }
    setHover(best);
  };

  return (
    <div className="st-area-wrap">
      <svg
        className="st-area-svg"
        viewBox={`0 0 ${W} ${H}`}
        preserveAspectRatio="none"
        onPointerMove={onMove}
        onPointerLeave={() => setHover(null)}
        style={{ height: `${H}px` }}
      >
        <defs>
          <linearGradient id={`area-${uid}`} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="rgba(59,130,246,0.45)" />
            <stop offset="100%" stopColor="rgba(59,130,246,0)" />
          </linearGradient>
        </defs>

        {/* Gridlines + max label */}
        {gridVals.map((gv, i) => (
          <g key={i}>
            <line x1={0} x2={W} y1={y(gv)} y2={y(gv)} className="st-grid-line" />
            <text x={6} y={y(gv) - 4} className="st-grid-label">{gv.toLocaleString()}</text>
          </g>
        ))}

        <path d={area} fill={`url(#area-${uid})`} />
        <path d={line} className="st-area-line" fill="none" />

        {/* Year tick labels */}
        {points.map((p, i) => {
          const t = tickEvery(p[0], i);
          if (!t) return null;
          return (
            <g key={i}>
              <line x1={x(i)} x2={x(i)} y1={padT} y2={baseY} className="st-grid-tick" />
              <text x={x(i)} y={H - 6} textAnchor="middle" className="st-tick">{t}</text>
            </g>
          );
        })}

        {/* Hover dot */}
        {hover !== null && (
          <g pointerEvents="none">
            <line x1={x(hover)} x2={x(hover)} y1={padT} y2={baseY} className="st-hover-line" />
            <circle cx={x(hover)} cy={y(points[hover][1])} r={4.5} className="st-hover-dot" />
          </g>
        )}

        {/* Native titles for accessibility / non-pointer */}
        {points.map((p, i) => (
          <rect key={`t${i}`} x={x(i) - W / n / 2} y={padT} width={W / n} height={plotH} fill="transparent">
            <title>{fmtValue(p[0], p[1])}</title>
          </rect>
        ))}
      </svg>
      <div className="st-area-readout">
        {hover !== null
          ? fmtValue(points[hover][0], points[hover][1])
          : `Peak ${max.toLocaleString()}`}
      </div>
    </div>
  );
}

// ── Radial 24-hour clock ────────────────────────────────────────────────────────

function RadialClock({ hours }: { hours: number[] }) {
  const total = hours.reduce((a, b) => a + b, 0);
  if (total === 0) return <p className="st-empty">No data</p>;

  const size = 220;
  const cx = size / 2;
  const cy = size / 2;
  const rInner = 46;
  const rMax = 96;
  const max = Math.max(...hours, 1);
  const peakHour = hours.indexOf(max);

  const seg = (2 * Math.PI) / 24;
  const gap = seg * 0.12;

  // angle: midnight (hour 0) at top, clockwise
  const ang = (h: number) => -Math.PI / 2 + h * seg;

  const arc = (h: number) => {
    const v = hours[h];
    const t = v / max;
    const rOuter = rInner + Math.max(4, t * (rMax - rInner));
    const a0 = ang(h) + gap / 2;
    const a1 = ang(h) + seg - gap / 2;
    const x0i = cx + rInner * Math.cos(a0);
    const y0i = cy + rInner * Math.sin(a0);
    const x1i = cx + rInner * Math.cos(a1);
    const y1i = cy + rInner * Math.sin(a1);
    const x0o = cx + rOuter * Math.cos(a0);
    const y0o = cy + rOuter * Math.sin(a0);
    const x1o = cx + rOuter * Math.cos(a1);
    const y1o = cy + rOuter * Math.sin(a1);
    return `M ${x0i} ${y0i} L ${x0o} ${y0o} A ${rOuter} ${rOuter} 0 0 1 ${x1o} ${y1o} L ${x1i} ${y1i} A ${rInner} ${rInner} 0 0 0 ${x0i} ${y0i} Z`;
  };

  const ringLabels = [
    { h: 0, txt: "0" },
    { h: 6, txt: "6" },
    { h: 12, txt: "12" },
    { h: 18, txt: "18" },
  ];

  return (
    <div className="st-clock-wrap">
      <svg viewBox={`0 0 ${size} ${size}`} className="st-clock-svg" style={{ maxWidth: `${size}px` }}>
        {hours.map((v, h) => (
          <path
            key={h}
            d={arc(h)}
            fill={intensityColor(v / max)}
            className="st-clock-seg"
          >
            <title>{`${String(h).padStart(2, "0")}:00–${String((h + 1) % 24).padStart(2, "0")}:00 · ${v.toLocaleString()} photos`}</title>
          </path>
        ))}
        {ringLabels.map(({ h, txt }) => {
          const a = ang(h) + seg / 2;
          const r = rMax + 12;
          return (
            <text
              key={h}
              x={cx + r * Math.cos(a)}
              y={cy + r * Math.sin(a)}
              textAnchor="middle"
              dominantBaseline="central"
              className="st-clock-ring-lbl"
            >
              {txt}
            </text>
          );
        })}
        <text x={cx} y={cy - 6} textAnchor="middle" className="st-clock-center-num">
          {`${String(peakHour).padStart(2, "0")}–${String((peakHour + 1) % 24).padStart(2, "0")}`}
        </text>
        <text x={cx} y={cy + 14} textAnchor="middle" className="st-clock-center-sub">
          favorite hour
        </text>
      </svg>
    </div>
  );
}

// ── Vertical gradient bars (weekday, focal, ratings) ────────────────────────────

interface VBarsProps {
  values: number[];
  labels: string[];
  titles?: string[];
  height?: number;
  peakLabel?: boolean;    // show peak count above the bar
  starLabels?: boolean;   // colour ★ in labels
}

function VBars({ values, labels, titles, height = 130, peakLabel = false, starLabels = false }: VBarsProps) {
  const uid = useId().replace(/:/g, "");
  const n = values.length;
  if (n === 0 || values.every((v) => v === 0)) return <p className="st-empty">No data</p>;

  const max = Math.max(...values, 1);
  const peakIdx = values.indexOf(max);
  const W = 100;
  const slot = W / n;
  const barW = slot * 0.62;
  const pad = (slot - barW) / 2;
  const topPad = peakLabel ? 14 : 4;
  const plotH = height - topPad;

  return (
    <div className="st-vbars">
      <svg viewBox={`0 0 ${W} ${height}`} preserveAspectRatio="none" className="st-vbars-svg" style={{ height: `${height}px` }}>
        <defs>
          <linearGradient id={`vb-${uid}`} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#3B82F6" />
            <stop offset="100%" stopColor="#2563EB" />
          </linearGradient>
          <linearGradient id={`vbp-${uid}`} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#93C5FD" />
            <stop offset="100%" stopColor="#3B82F6" />
          </linearGradient>
        </defs>
        {values.map((v, i) => {
          const h = (v / max) * plotH;
          const x = i * slot + pad;
          const yTop = topPad + plotH - h;
          const isPeak = i === peakIdx;
          return (
            <g key={i}>
              <rect
                x={x}
                y={yTop}
                width={barW}
                height={Math.max(h, 0.5)}
                rx={1.4}
                fill={`url(#${isPeak ? "vbp" : "vb"}-${uid})`}
              >
                {titles?.[i] && <title>{titles[i]}</title>}
              </rect>
              {peakLabel && isPeak && v > 0 && (
                <text x={x + barW / 2} y={yTop - 3} textAnchor="middle" className="st-vbar-peaknum">
                  {v.toLocaleString()}
                </text>
              )}
            </g>
          );
        })}
      </svg>
      <div className="st-vbars-labels">
        {labels.map((l, i) => (
          <span key={i} className={`st-vbar-lbl${starLabels ? " st-star" : ""}`}>{l}</span>
        ))}
      </div>
    </div>
  );
}

// ── Camera share donut ──────────────────────────────────────────────────────────

function CameraDonut({ cameras }: { cameras: [string, number][] }) {
  const total = cameras.reduce((a, [, c]) => a + c, 0);
  if (total === 0) return null;

  const top = cameras.slice(0, 5);
  const otherCount = cameras.slice(5).reduce((a, [, c]) => a + c, 0);
  const slices = [...top.map(([name, c]) => ({ name, count: c }))];
  if (otherCount > 0) slices.push({ name: "Other", count: otherCount });

  const size = 150;
  const cx = size / 2;
  const cy = size / 2;
  const r = 60;
  const sw = 22;

  let acc = 0;
  const circ = 2 * Math.PI * r;
  const topShare = Math.round((top[0][1] / total) * 100);

  return (
    <div className="st-donut-wrap">
      <svg viewBox={`0 0 ${size} ${size}`} className="st-donut-svg" style={{ maxWidth: `${size}px` }}>
        <circle cx={cx} cy={cy} r={r} fill="none" stroke="#243049" strokeWidth={sw} />
        {slices.map((s, i) => {
          const frac = s.count / total;
          const dash = frac * circ;
          const offset = -acc * circ;
          acc += frac;
          return (
            <circle
              key={i}
              cx={cx}
              cy={cy}
              r={r}
              fill="none"
              stroke={DONUT_HUES[i % DONUT_HUES.length]}
              strokeWidth={sw}
              strokeDasharray={`${dash} ${circ - dash}`}
              strokeDashoffset={offset}
              transform={`rotate(-90 ${cx} ${cy})`}
            >
              <title>{`${s.name} · ${s.count.toLocaleString()} (${Math.round(frac * 100)}%)`}</title>
            </circle>
          );
        })}
        <text x={cx} y={cy - 4} textAnchor="middle" className="st-donut-num">{topShare}%</text>
        <text x={cx} y={cy + 13} textAnchor="middle" className="st-donut-sub">top camera</text>
      </svg>
      <div className="st-donut-legend">
        {slices.map((s, i) => (
          <div key={i} className="st-legend-chip" title={`${s.name} · ${s.count.toLocaleString()}`}>
            <span className="st-legend-dot" style={{ background: DONUT_HUES[i % DONUT_HUES.length] }} />
            <span className="st-legend-name">{s.name}</span>
            <span className="st-legend-pct">{Math.round((s.count / total) * 100)}%</span>
          </div>
        ))}
      </div>
    </div>
  );
}

// ── Ranked list ─────────────────────────────────────────────────────────────────

interface RankRow {
  label: string;
  count: number;
  title?: string;
  onClick?: () => void;
}

function RankList({
  rows,
  hue,
  total,
  cap,
}: {
  rows: RankRow[];
  hue: "blue" | "violet" | "teal";
  total: number;
  cap?: number;
}) {
  if (rows.length === 0) return <p className="st-empty">No data</p>;
  const max = Math.max(...rows.map((r) => r.count), 1);
  const shown = cap ? rows.slice(0, cap) : rows;
  const hidden = cap ? rows.length - shown.length : 0;

  return (
    <div className="st-rank-list">
      {shown.map((row, i) => (
        <button
          key={i}
          className={`st-rank-row st-hue-${hue}${row.onClick ? " clickable" : ""}${i < 3 ? " top3" : ""}`}
          onClick={row.onClick}
          title={row.title}
          disabled={!row.onClick}
        >
          <span className="st-rank-num">{i + 1}</span>
          <span className="st-rank-name">{row.label}</span>
          <span className="st-rank-track">
            <span className="st-rank-fill" style={{ width: `${(row.count / max) * 100}%` }} />
          </span>
          <span className="st-rank-count">{row.count.toLocaleString()}</span>
          <span className="st-rank-pct">{total > 0 ? Math.round((row.count / total) * 100) : 0}%</span>
        </button>
      ))}
      {hidden > 0 && <div className="st-rank-more">and {hidden.toLocaleString()} more…</div>}
    </div>
  );
}

// ── StatsView ─────────────────────────────────────────────────────────────────

const WEEKDAY_LABELS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const WEEKDAY_ORDER = [1, 2, 3, 4, 5, 6, 0];

function StatsView({ api }: { api: ChairPhotoAPI }) {
  const [stats, setStats] = useState<CatalogStats | null>(null);
  const [error, setError] = useState("");
  // Cache of all tags — fetched once and used to resolve tag names for the scope chip.
  const tagCacheRef = useRef<Map<number, string>>(new Map());

  // Read the current filter context each render (api.getFilterContext() reads a
  // module-level variable updated by App's useEffect + notify(), so after the first
  // render following a scope change, the value is current).
  const ctx = api.getFilterContext();

  // Fetch tags once on mount so we can show tag names in the scope chip.
  useEffect(() => {
    api.listTags().then((tags) => {
      const m = new Map<number, string>();
      for (const t of tags) m.set(t.id, t.name);
      tagCacheRef.current = m;
    }).catch(() => {});
  }, [api]);

  useEffect(() => {
    let alive = true;
    setError("");
    setStats(null);
    const args: Record<string, unknown> = {
      tagId: ctx.tagId ?? null,
      albumId: ctx.albumId ?? null,
      batchId: ctx.batchId ?? null,
    };
    api
      .invoke<CatalogStats>("catalog_stats", args)
      .then((s) => { if (alive) setStats(s); })
      .catch((e) => { if (alive) setError(String(e)); });
    return () => { alive = false; };
  // Re-fetch whenever the scope changes.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [api, ctx.tagId, ctx.albumId, ctx.batchId]);

  // Build the scope label for the chip (shown when any scope is active).
  const scopeLabel: string | null = (() => {
    if (ctx.tagId != null) {
      const name = tagCacheRef.current.get(ctx.tagId);
      return name ? `Scoped to tag ‹${name}›` : "Scoped to tag";
    }
    if (ctx.albumId != null) return "Scoped to current album";
    if (ctx.batchId != null) return "Scoped to import batch";
    return null;
  })();

  const scopeChip = scopeLabel ? (
    <div className="st-scope-chip">
      <span className="st-scope-label">{scopeLabel}</span>
      <span className="st-scope-hint">Select "All photos" in the sidebar to clear</span>
    </div>
  ) : null;

  if (error) {
    return (
      <div className="st-root">
        <div className="st-inner">
          {scopeChip}
          <div className="st-error">{error}</div>
        </div>
      </div>
    );
  }

  if (!stats) {
    return (
      <div className="st-root">
        <div className="st-inner">
          {scopeChip}
          <p className="st-loading">Loading statistics…</p>
        </div>
      </div>
    );
  }

  // ── Derived data ────────────────────────────────────────────────────────────

  const dateRange =
    stats.firstMonth && stats.lastMonth
      ? `${fmtMonth(stats.firstMonth)} – ${fmtMonth(stats.lastMonth)}`
      : "—";

  const monthly =
    stats.firstMonth && stats.lastMonth
      ? buildTimeline(stats.timeline, stats.firstMonth, stats.lastMonth)
      : stats.timeline;

  // Adaptive: if span > 180 months, aggregate to years.
  const yearly = monthly.length > 180;
  const timelinePoints: [string, number][] = yearly ? aggregateToYears(monthly) : monthly;

  const tickEvery = yearly
    ? (label: string, i: number) => {
        // Year labels: show a subset so ticks don't crowd.
        const step = Math.ceil(timelinePoints.length / 12);
        return i % step === 0 ? label : "";
      }
    : (label: string) => {
        const { month, year } = parseYM(label);
        return month === 1 ? String(year) : "";
      };

  const fmtValue = yearly
    ? (label: string, v: number) => `${label} · ${v.toLocaleString()} photos`
    : (label: string, v: number) => `${fmtMonth(label)} · ${v.toLocaleString()} photos`;

  // Busiest year (from monthly aggregation, always)
  const yearTotals = aggregateToYears(monthly);
  const busiestYear = yearTotals.reduce<[string, number] | null>(
    (best, cur) => (!best || cur[1] > best[1] ? cur : best),
    null,
  );

  // Weekend vs weekday
  const wdTotal = stats.weekdays.reduce((a, b) => a + b, 0);
  const weekendCount = (stats.weekdays[0] ?? 0) + (stats.weekdays[6] ?? 0); // Sun + Sat
  const weekendPct = wdTotal > 0 ? Math.round((weekendCount / wdTotal) * 100) : 0;
  const weekendFraming =
    weekendPct >= 45
      ? { label: "Weekend shooter", value: `${weekendPct}% on Sat/Sun` }
      : { label: "Weekday shooter", value: `${100 - weekendPct}% on weekdays` };

  // Favorite hour
  const hourMax = Math.max(...stats.hours, 0);
  const peakHour = stats.hours.indexOf(hourMax);
  const favHourVal =
    hourMax > 0
      ? `${String(peakHour).padStart(2, "0")}:00–${String((peakHour + 1) % 24).padStart(2, "0")}:00`
      : "—";

  // Fun facts
  const busiestDay = stats.topDays[0] ?? null;

  // Ranked rows
  const tagRows: RankRow[] = stats.topTags.map((t) => ({
    label: t.label.split("/").pop() ?? t.label,
    count: t.count,
    title: t.label,
    onClick: () => api.filterByTag(t.id),
  }));
  const cameraRows: RankRow[] = stats.cameras.map(([name, count]) => ({ label: name, count }));
  const lensRows: RankRow[] = stats.lenses.map(([name, count]) => ({ label: name, count }));

  const tagTotal = stats.topTags.reduce((a, t) => a + t.count, 0) || stats.totalPhotos;
  const cameraTotal = stats.cameras.reduce((a, [, c]) => a + c, 0);
  const lensTotal = stats.lenses.reduce((a, [, c]) => a + c, 0);

  // Weekdays Mon-first
  const wdValues = WEEKDAY_ORDER.map((i) => stats.weekdays[i] ?? 0);
  const wdTitles = WEEKDAY_LABELS.map((d, i) => `${d} · ${wdValues[i].toLocaleString()} photos`);

  // Focal
  const flRaw = bucketFocalLengths(stats.focalLengths);
  const { labels: flLabels, values: flValues } = trimZeroBuckets(
    FL_BUCKETS.map((b) => b.label),
    flRaw,
  );
  const flTitles = flLabels.map((lbl, i) => `${lbl}mm · ${flValues[i].toLocaleString()} photos`);

  // Ratings
  const ratingLabels = ["—", "★", "★★", "★★★", "★★★★", "★★★★★"];
  const ratingTitles = stats.ratings.map(
    (count, i) => `${i === 0 ? "Unrated" : `${i}★`} · ${count.toLocaleString()} photos`,
  );

  // ── Render ──────────────────────────────────────────────────────────────────

  return (
    <div className="st-root">
      <div className="st-inner">

        {/* Scope chip — shown when the sidebar has an active tag / album / batch filter */}
        {scopeChip}

        {/* Header — 4 stat cards */}
        <div className="st-header">
          <div className="st-stat-card st-accent-blue">
            <div className="st-stat-num">{stats.totalPhotos.toLocaleString()}</div>
            <div className="st-stat-lbl">Photos</div>
          </div>
          <div className="st-stat-card st-accent-violet">
            <div className="st-stat-num st-stat-sm">{dateRange}</div>
            <div className="st-stat-lbl">Date range</div>
          </div>
          <div className="st-stat-card st-accent-amber">
            <div className="st-stat-num">{stats.cameras.length.toLocaleString()}</div>
            <div className="st-stat-lbl">Cameras</div>
          </div>
          <div className="st-stat-card st-accent-teal">
            <div className="st-stat-num">{stats.lenses.length.toLocaleString()}</div>
            <div className="st-stat-lbl">Lenses</div>
          </div>
        </div>

        {/* Fun facts strip */}
        <div className="st-facts">
          <div className="st-fact">
            <span className="st-fact-emoji">📅</span>
            <div className="st-fact-body">
              <div className="st-fact-lbl">Busiest day</div>
              <div className="st-fact-val">
                {busiestDay
                  ? `${fmtDay(busiestDay[0])} · ${busiestDay[1].toLocaleString()}`
                  : "—"}
              </div>
            </div>
          </div>
          <div className="st-fact">
            <span className="st-fact-emoji">🗓️</span>
            <div className="st-fact-body">
              <div className="st-fact-lbl">Busiest year</div>
              <div className="st-fact-val">
                {busiestYear ? `${busiestYear[0]} · ${busiestYear[1].toLocaleString()}` : "—"}
              </div>
            </div>
          </div>
          <div className="st-fact">
            <span className="st-fact-emoji">🕐</span>
            <div className="st-fact-body">
              <div className="st-fact-lbl">Favorite hour</div>
              <div className="st-fact-val">{favHourVal}</div>
            </div>
          </div>
          <div className="st-fact">
            <span className="st-fact-emoji">🌤️</span>
            <div className="st-fact-body">
              <div className="st-fact-lbl">{weekendFraming.label}</div>
              <div className="st-fact-val">{weekendFraming.value}</div>
            </div>
          </div>
        </div>

        {/* Timeline — hero area chart */}
        <div className="st-card">
          <div className="st-panel-head">
            <span>Photos per {yearly ? "year" : "month"}</span>
          </div>
          {timelinePoints.length > 0 ? (
            <AreaChart
              points={timelinePoints}
              tickEvery={tickEvery}
              fmtValue={fmtValue}
              height={200}
            />
          ) : (
            <p className="st-empty">No data</p>
          )}
          {stats.invalidDates > 0 && (
            <div className="st-footnote">
              {stats.invalidDates.toLocaleString()} photos with invalid dates excluded
            </div>
          )}
        </div>

        {/* Time of day + Day of week */}
        <div className="st-grid">
          <div className="st-card">
            <div className="st-panel-head">Time of day</div>
            <RadialClock hours={stats.hours} />
          </div>

          <div className="st-card">
            <div className="st-panel-head">Day of week</div>
            <VBars
              values={wdValues}
              labels={WEEKDAY_LABELS}
              titles={wdTitles}
              height={170}
              peakLabel
            />
          </div>
        </div>

        {/* Camera donut + camera list */}
        <div className="st-card">
          <div className="st-panel-head">Cameras</div>
          <div className="st-camera-split">
            <CameraDonut cameras={stats.cameras} />
            <div className="st-camera-list">
              <RankList rows={cameraRows} hue="violet" total={cameraTotal} cap={12} />
            </div>
          </div>
        </div>

        {/* Tags + Lenses */}
        <div className="st-grid">
          <div className="st-card">
            <div className="st-panel-head">Top tags</div>
            <RankList rows={tagRows} hue="blue" total={tagTotal} />
          </div>

          <div className="st-card">
            <div className="st-panel-head">Lenses</div>
            <RankList rows={lensRows} hue="teal" total={lensTotal} cap={12} />
          </div>

          {/* Focal length */}
          <div className="st-card">
            <div className="st-panel-head">Focal length (mm)</div>
            <VBars values={flValues} labels={flLabels} titles={flTitles} height={150} />
          </div>

          {/* Ratings */}
          <div className="st-card">
            <div className="st-panel-head">Ratings</div>
            <VBars
              values={stats.ratings}
              labels={ratingLabels}
              titles={ratingTitles}
              height={150}
              starLabels
            />
          </div>
        </div>
      </div>
    </div>
  );
}

// ── Module export ─────────────────────────────────────────────────────────────

export const statisticsModule: ChairPhotoModule = {
  id: "statistics",
  name: "Statistics",
  version: "0.2.0",
  description:
    "Dashboard of catalog stats — timeline, top tags, cameras, lenses, and shooting habits.",
  onLoad(api) {
    api.registerMainView({
      id: "statistics",
      label: "Stats",
      // Bar-chart glyph, matching the app's 13px stroke icon language.
      icon: (
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
          <line x1="6" y1="20" x2="6" y2="14" />
          <line x1="12" y1="20" x2="12" y2="4" />
          <line x1="18" y1="20" x2="18" y2="10" />
        </svg>
      ),
      render: () => <StatsView api={api} />,
    });
  },
};

// Gate 0 of the GPU-smoothness work (docs/plans/darkroom/00-status.md): a probe that
// answers one question on THIS machine — does WebKitGTK hand us a WebGL2 context that
// completes a 1400 px look-shader frame at display rate? Tauri's Linux-graphics guide
// warns that context creation "succeeds even when the result is backed by a software
// rasterizer or a slow presentation path. There is no error to catch" — so the only
// honest answer is a measurement: draw a representative frame (tone math, eight 3D-LUT
// fetches, an integer-hash grain) for five seconds and report the rAF cadence and the
// `gl.finish()` time. Dev-only; opened from Preferences → Darkroom when render timing
// logging is on. The report is logged as one `[gl-spike]` line and shown in the modal.
import { useEffect, useRef, useState } from "react";
import { p50p95 } from "./renderTiming";
import "./darkroom.css";

export const SPIKE_W = 1400;
export const SPIKE_H = 933;
/** 3D LUT edge — the size of a typical .cube film emulation. */
const SPIKE_LUT = 33;
const SPIKE_MS = 5000;

export interface SpikeReport {
  context: boolean;
  renderer: string;
  maxTexture: number;
  max3d: number;
  frames: number;
  /** rAF-to-rAF interval while drawing, ms. */
  cadence: { p50: number; p95: number };
  /** draw + gl.finish() per frame, ms. */
  finish: { p50: number; p95: number };
  lost: boolean;
  error?: string;
}

const VERT = `#version 300 es
void main() {
  // Fullscreen triangle from gl_VertexID — no vertex buffer needed.
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

// A stand-in for the real look shader with the same cost profile: per-pixel tone math,
// a manual trilinear 3D-LUT fetch (eight texelFetches), and the integer grain hash.
const FRAG = `#version 300 es
precision highp float;
precision highp int;
precision highp sampler3D;
uniform sampler2D uBase;
uniform highp sampler3D uLut;
uniform ivec2 uSize;
uniform float uEv;
uniform float uContrast;
uniform uint uSeed;
out vec4 outColor;

float lum(vec3 c) { return dot(c, vec3(0.299, 0.587, 0.114)); }

float hash2(int x, int y, uint seed) {
  uint h = uint(x) * 0x9E3779B1u + uint(y) * 0x85EBCA77u + seed * 0xC2B2AE3Du;
  h ^= h >> 15u; h *= 0x2C1B3C6Du; h ^= h >> 12u; h *= 0x29712D39u; h ^= h >> 15u;
  return float(h) / 4294967296.0 * 2.0 - 1.0;
}

vec3 lut3(vec3 c) {
  int n = textureSize(uLut, 0).x;
  vec3 x = clamp(c, 0.0, 1.0) * float(n - 1);
  ivec3 i0 = ivec3(floor(x));
  ivec3 i1 = min(i0 + 1, ivec3(n - 1));
  vec3 t = x - vec3(i0);
  vec3 c00 = mix(texelFetch(uLut, ivec3(i0.x, i0.y, i0.z), 0).rgb, texelFetch(uLut, ivec3(i1.x, i0.y, i0.z), 0).rgb, t.x);
  vec3 c10 = mix(texelFetch(uLut, ivec3(i0.x, i1.y, i0.z), 0).rgb, texelFetch(uLut, ivec3(i1.x, i1.y, i0.z), 0).rgb, t.x);
  vec3 c01 = mix(texelFetch(uLut, ivec3(i0.x, i0.y, i1.z), 0).rgb, texelFetch(uLut, ivec3(i1.x, i0.y, i1.z), 0).rgb, t.x);
  vec3 c11 = mix(texelFetch(uLut, ivec3(i0.x, i1.y, i1.z), 0).rgb, texelFetch(uLut, ivec3(i1.x, i1.y, i1.z), 0).rgb, t.x);
  return mix(mix(c00, c10, t.y), mix(c01, c11, t.y), t.z);
}

void main() {
  ivec2 p = ivec2(int(gl_FragCoord.x), uSize.y - 1 - int(gl_FragCoord.y));
  vec3 c = texelFetch(uBase, p, 0).rgb;
  c *= exp2(uEv);
  c = (c - 0.5) * uContrast + 0.5;
  c = lut3(c);
  float l = clamp(lum(c), 0.0, 1.0);
  c += hash2(p.x, p.y, uSeed) * 0.12 * 4.0 * l * (1.0 - l);
  outColor = vec4(clamp(c, 0.0, 1.0), 1.0);
}`;

function compile(gl: WebGL2RenderingContext, type: number, src: string): WebGLShader {
  const sh = gl.createShader(type);
  if (!sh) throw new Error("createShader failed");
  gl.shaderSource(sh, src);
  gl.compileShader(sh);
  if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
    throw new Error(`shader: ${gl.getShaderInfoLog(sh) ?? "unknown error"}`);
  }
  return sh;
}

/** A colourful test card: a horizontal luminance ramp over vertical RGB bands, so the
 *  LUT fetches spread across the lattice rather than hitting one cell. */
function testCard(w: number, h: number): Uint8Array {
  const px = new Uint8Array(w * h * 4);
  for (let y = 0; y < h; y++) {
    const band = Math.floor((y * 3) / h);
    for (let x = 0; x < w; x++) {
      const l = Math.round((x * 255) / (w - 1));
      const o = (y * w + x) * 4;
      px[o] = band === 0 ? l : band === 1 ? l >> 2 : l >> 1;
      px[o + 1] = band === 0 ? l >> 1 : band === 1 ? l : l >> 2;
      px[o + 2] = band === 0 ? l >> 2 : band === 1 ? l >> 1 : l;
      px[o + 3] = 255;
    }
  }
  return px;
}

/** An identity 3D LUT in .cube order (R fastest), RGBA32F. */
function identityLut(n: number): Float32Array {
  const data = new Float32Array(n * n * n * 4);
  let i = 0;
  for (let b = 0; b < n; b++)
    for (let g = 0; g < n; g++)
      for (let r = 0; r < n; r++) {
        data[i++] = r / (n - 1);
        data[i++] = g / (n - 1);
        data[i++] = b / (n - 1);
        data[i++] = 1;
      }
  return data;
}

/** Run the probe on `canvas`; `done` receives the report once. Returns a cancel fn. */
export function runSpike(canvas: HTMLCanvasElement, done: (r: SpikeReport) => void): () => void {
  let cancelled = false;
  let lost = false;
  const cancel = () => {
    cancelled = true;
  };
  const base: SpikeReport = {
    context: false,
    renderer: "unavailable",
    maxTexture: 0,
    max3d: 0,
    frames: 0,
    cadence: { p50: NaN, p95: NaN },
    finish: { p50: NaN, p95: NaN },
    lost: false,
  };
  const finish = (r: SpikeReport) => {
    console.debug(`[gl-spike] ${JSON.stringify(r)}`);
    done(r);
  };

  canvas.width = SPIKE_W;
  canvas.height = SPIKE_H;
  const gl = canvas.getContext("webgl2", {
    alpha: false,
    antialias: false,
    depth: false,
    stencil: false,
    premultipliedAlpha: false,
    preserveDrawingBuffer: false,
  });
  if (!gl) {
    finish({ ...base, error: "getContext('webgl2') returned null" });
    return cancel;
  }
  canvas.addEventListener("webglcontextlost", (e) => {
    e.preventDefault();
    lost = true;
  });
  const dbg = gl.getExtension("WEBGL_debug_renderer_info");
  const renderer = dbg
    ? String(gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL))
    : `masked (${String(gl.getParameter(gl.RENDERER))})`;
  const info = {
    ...base,
    context: true,
    renderer,
    maxTexture: Number(gl.getParameter(gl.MAX_TEXTURE_SIZE)),
    max3d: Number(gl.getParameter(gl.MAX_3D_TEXTURE_SIZE)),
  };

  let program: WebGLProgram | null = null;
  try {
    program = gl.createProgram();
    if (!program) throw new Error("createProgram failed");
    gl.attachShader(program, compile(gl, gl.VERTEX_SHADER, VERT));
    gl.attachShader(program, compile(gl, gl.FRAGMENT_SHADER, FRAG));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      throw new Error(`link: ${gl.getProgramInfoLog(program) ?? "unknown error"}`);
    }
  } catch (e) {
    finish({ ...info, error: String(e) });
    return cancel;
  }
  gl.useProgram(program);

  const baseTex = gl.createTexture();
  gl.activeTexture(gl.TEXTURE0);
  gl.bindTexture(gl.TEXTURE_2D, baseTex);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, SPIKE_W, SPIKE_H, 0, gl.RGBA, gl.UNSIGNED_BYTE, testCard(SPIKE_W, SPIKE_H));

  const lutTex = gl.createTexture();
  gl.activeTexture(gl.TEXTURE1);
  gl.bindTexture(gl.TEXTURE_3D, lutTex);
  gl.texParameteri(gl.TEXTURE_3D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_3D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_3D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_3D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_3D, gl.TEXTURE_WRAP_R, gl.CLAMP_TO_EDGE);
  gl.texImage3D(gl.TEXTURE_3D, 0, gl.RGBA32F, SPIKE_LUT, SPIKE_LUT, SPIKE_LUT, 0, gl.RGBA, gl.FLOAT, identityLut(SPIKE_LUT));

  gl.uniform1i(gl.getUniformLocation(program, "uBase"), 0);
  gl.uniform1i(gl.getUniformLocation(program, "uLut"), 1);
  gl.uniform2i(gl.getUniformLocation(program, "uSize"), SPIKE_W, SPIKE_H);
  const uEv = gl.getUniformLocation(program, "uEv");
  const uContrast = gl.getUniformLocation(program, "uContrast");
  const uSeed = gl.getUniformLocation(program, "uSeed");
  gl.viewport(0, 0, SPIKE_W, SPIKE_H);
  const glError = gl.getError();
  if (glError !== gl.NO_ERROR) {
    finish({ ...info, error: `GL error after setup: 0x${glError.toString(16)}` });
    return cancel;
  }

  const cadence: number[] = [];
  const finishes: number[] = [];
  let frames = 0;
  let lastFrame = 0;
  const t0 = performance.now();
  const step = (now: number) => {
    if (cancelled) return;
    if (lastFrame) cadence.push(now - lastFrame);
    lastFrame = now;
    const phase = (now - t0) / 1000;
    // Animated uniforms: the frame must actually change, or a driver may elide it.
    gl.uniform1f(uEv, Math.sin(phase * 3) * 0.8);
    gl.uniform1f(uContrast, 1 + Math.cos(phase * 2) * 0.3);
    gl.uniform1ui(uSeed, frames >>> 0);
    const d0 = performance.now();
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    gl.finish();
    finishes.push(performance.now() - d0);
    frames++;
    if (now - t0 < SPIKE_MS && !lost) {
      requestAnimationFrame(step);
    } else {
      finish({ ...info, frames, cadence: p50p95(cadence), finish: p50p95(finishes), lost });
    }
  };
  requestAnimationFrame(step);
  return cancel;
}

export function GlSpike({
  onClose,
  onReport,
}: {
  onClose: () => void;
  /** Each completed run, as the JSON line that was also logged — the caller persists it. */
  onReport?: (json: string) => void;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [report, setReport] = useState<SpikeReport | null>(null);
  const [run, setRun] = useState(0);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    setReport(null);
    let alive = true;
    const cancel = runSpike(canvas, (r) => {
      if (!alive) return;
      setReport(r);
      onReport?.(JSON.stringify(r));
    });
    return () => {
      alive = false;
      cancel();
    };
    // A run is keyed by `run` alone; onReport is read at completion time.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [run]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const ms = (v: number) => (Number.isNaN(v) ? "—" : `${v.toFixed(1)} ms`);
  return (
    <div className="dk-spike-backdrop" onClick={onClose}>
      <div className="dk-spike" onClick={(e) => e.stopPropagation()}>
        <div>
          <b>WebGL probe</b>{" "}
          <span className="modal-sub">
            {SPIKE_W}×{SPIKE_H} look-shader frames for {SPIKE_MS / 1000} s — cadence and
            gl.finish() decide whether a GPU drag tier is worth building here
          </span>
        </div>
        <canvas ref={canvasRef} key={run} />
        {report ? (
          <table>
            <tbody>
              <tr>
                <td>Context</td>
                <td>{report.context ? "webgl2" : "none"}</td>
              </tr>
              <tr>
                <td>Renderer</td>
                <td>{report.renderer}</td>
              </tr>
              <tr>
                <td>Max texture / 3D</td>
                <td>
                  {report.maxTexture} / {report.max3d}
                </td>
              </tr>
              <tr>
                <td>Frames</td>
                <td>{report.frames}</td>
              </tr>
              <tr>
                <td>rAF cadence p50 / p95</td>
                <td>
                  {ms(report.cadence.p50)} / {ms(report.cadence.p95)}
                </td>
              </tr>
              <tr>
                <td>draw + finish p50 / p95</td>
                <td>
                  {ms(report.finish.p50)} / {ms(report.finish.p95)}
                </td>
              </tr>
              {report.lost && (
                <tr>
                  <td>Context lost</td>
                  <td>yes</td>
                </tr>
              )}
              {report.error && (
                <tr>
                  <td>Error</td>
                  <td>{report.error}</td>
                </tr>
              )}
            </tbody>
          </table>
        ) : (
          <div className="modal-sub">measuring…</div>
        )}
        <div className="dk-spike-foot">
          <button onClick={() => setRun((n) => n + 1)} disabled={!report}>
            Run again
          </button>
          <button onClick={onClose}>Close</button>
        </div>
      </div>
    </div>
  );
}

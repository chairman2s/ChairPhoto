#!/usr/bin/env python3
"""Reproduce the WebKitGTK + NVIDIA WebGL teardown crash outside the app.

Found while measuring the Darkroom's WebGL drag tier (docs/plans/darkroom/00-status.md,
"Follow-on: GPU smoothness"): with `WEBKIT_DISABLE_DMABUF_RENDERER=1` — what lib.rs sets on
Linux — a page torn down while a *visible* canvas still holds a live WebGL2 context
segfaults the WebKitWebProcess inside libnvidia-eglcore (same stack as the two app crashes
of 2026-09-06). A context released while the page lives, a hidden canvas, or the DMABUF
renderer left on are all clean.

Usage (from anywhere; opens a small window with --show, else stays hidden):

    python3 scripts/webgl-teardown-repro.py <mode> [cycles] [--show]

  mode   drop           create, draw, remove the canvas — clean
         lose           create, draw, WEBGL_lose_context.loseContext() — clean
         keep           contexts stay alive until the page dies — crashes at exit (--show)
         keep-pagehide  keep, but lose them in pagehide/beforeunload — still crashes:
                        those handlers run too late for the view's destruction

Check with `coredumpctl list --since=-2min` after a run: the crash happens during
teardown, after the RESULT line, so the web-process-terminated signal never fires.
Override the renderer to compare: WEBKIT_DISABLE_DMABUF_RENDERER=0 __NV_DISABLE_EXPLICIT_SYNC=1.
Needs python-gobject and webkit2gtk-4.1 (WebKitGTK 2.52.6 / NVIDIA 610.57 when recorded).
"""
import os, sys, time
os.environ.setdefault("WEBKIT_DISABLE_DMABUF_RENDERER", "1")
import gi
gi.require_version("Gtk", "3.0"); gi.require_version("WebKit2", "4.1")
from gi.repository import Gtk, WebKit2, GLib

MODE = sys.argv[1] if len(sys.argv) > 1 else "drop"   # drop | lose | keep
CYCLES = int(sys.argv[2]) if len(sys.argv) > 2 else 12
SHOW = "--show" in sys.argv

HTML = """<!doctype html><html><body><script>
const MODE = %r, CYCLES = %d;
const W = 1400, H = 933, N = 33;
const VERT = `#version 300 es\nvoid main(){vec2 p=vec2(float((gl_VertexID<<1)&2),float(gl_VertexID&2));gl_Position=vec4(p*2.0-1.0,0.0,1.0);}`;
const FRAG = `#version 300 es\nprecision highp float;precision highp sampler3D;uniform sampler2D uBase;uniform highp sampler3D uLut;uniform ivec2 uSize;uniform float uEv;out vec4 o;
void main(){ivec2 p=ivec2(int(gl_FragCoord.x),uSize.y-1-int(gl_FragCoord.y));vec3 c=texelFetch(uBase,p,0).rgb*exp2(uEv);int n=textureSize(uLut,0).x;vec3 x=clamp(c,0.0,1.0)*float(n-1);ivec3 i0=ivec3(floor(x));c=texelFetch(uLut,i0,0).rgb;o=vec4(c,1.0);}`;
let report = [];
function say(s){ report.push(s); document.title = "R:" + report.join("|"); }
function compile(gl,t,src){const s=gl.createShader(t);gl.shaderSource(s,src);gl.compileShader(s);if(!gl.getShaderParameter(s,gl.COMPILE_STATUS))throw new Error(gl.getShaderInfoLog(s));return s;}
function cycle(i){
  const canvas = document.createElement("canvas"); canvas.width=W; canvas.height=H;
  if (MODE !== "hidden") document.body.appendChild(canvas);
  const gl = canvas.getContext("webgl2",{alpha:false,antialias:false,depth:false,stencil:false,premultipliedAlpha:false});
  if(!gl){ say("nogl"); return null; }
  const prog=gl.createProgram(); gl.attachShader(prog,compile(gl,gl.VERTEX_SHADER,VERT)); gl.attachShader(prog,compile(gl,gl.FRAGMENT_SHADER,FRAG)); gl.linkProgram(prog); gl.useProgram(prog);
  const base=gl.createTexture(); gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D,base);
  gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_MIN_FILTER,gl.NEAREST); gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_MAG_FILTER,gl.NEAREST);
  gl.texImage2D(gl.TEXTURE_2D,0,gl.RGBA8,W,H,0,gl.RGBA,gl.UNSIGNED_BYTE,new Uint8Array(W*H*4).fill(128));
  const lut=gl.createTexture(); gl.activeTexture(gl.TEXTURE1); gl.bindTexture(gl.TEXTURE_3D,lut);
  gl.texParameteri(gl.TEXTURE_3D,gl.TEXTURE_MIN_FILTER,gl.NEAREST); gl.texParameteri(gl.TEXTURE_3D,gl.TEXTURE_MAG_FILTER,gl.NEAREST);
  gl.texImage3D(gl.TEXTURE_3D,0,gl.RGBA32F,N,N,N,0,gl.RGBA,gl.FLOAT,new Float32Array(N*N*N*4).fill(0.5));
  gl.uniform1i(gl.getUniformLocation(prog,"uBase"),0); gl.uniform1i(gl.getUniformLocation(prog,"uLut"),1); gl.uniform2i(gl.getUniformLocation(prog,"uSize"),W,H);
  const uEv=gl.getUniformLocation(prog,"uEv"); gl.viewport(0,0,W,H);
  for(let f=0;f<20;f++){ gl.uniform1f(uEv,Math.sin(f/3)); gl.drawArrays(gl.TRIANGLES,0,3); gl.finish(); }
  const err = gl.getError();
  say("c"+i+(err?":err"+err:""));
  return {canvas, gl};
}
let live = [];
let i = 0;
// keep-pagehide: contexts stay alive while the page lives, but are explicitly lost when
// the page goes away — the mitigation a long-lived GL drag tier would ship.
if (MODE === "keep-pagehide") {
  const loseAll = () => { for (const c of live) { const e = c.gl.getExtension("WEBGL_lose_context"); if (e) e.loseContext(); } live = []; };
  window.addEventListener("pagehide", loseAll);
  window.addEventListener("beforeunload", loseAll);
}
function step(){
  if (i >= CYCLES) { say("done"); return; }
  const ctx = cycle(i++);
  if (!ctx) return;
  if (MODE === "lose") { ctx.gl.getExtension("WEBGL_lose_context").loseContext(); }
  if (MODE === "keep" || MODE === "keep-pagehide") { live.push(ctx); }  // contexts stay alive
  else { ctx.canvas.remove(); }                          // drop; GC pressure below
  // GC pressure so dropped contexts actually get destroyed soon.
  let junk = []; for (let k = 0; k < 40; k++) junk.push(new Uint8Array(4<<20)); junk = null;
  setTimeout(step, 150);
}
setTimeout(step, 100);
</script></body></html>""" % (MODE, CYCLES)

win = Gtk.Window(title="webgl-repro"); win.set_default_size(320, 200)
view = WebKit2.WebView()
win.add(view)
state = {"crash": None, "title": "", "t0": time.time()}
def on_title(v, _p):
    t = v.get_title() or ""
    if t.startswith("R:"):
        state["title"] = t
        print(f"[{time.time()-state['t0']:.1f}s] {t}", flush=True)
        if t.endswith("|done") or t.endswith("R:done"):
            GLib.timeout_add(1500, finish)  # give GC/teardown a moment after the last cycle
def on_terminated(v, reason):
    state["crash"] = str(reason)
    print(f"[{time.time()-state['t0']:.1f}s] WEB PROCESS TERMINATED: {reason}", flush=True)
    GLib.timeout_add(200, finish)
def finish():
    Gtk.main_quit(); return False
view.connect("notify::title", on_title)
view.connect("web-process-terminated", on_terminated)
view.load_html(HTML, "about:blank")
if SHOW: win.show_all()
else: view.show(); win.realize()
GLib.timeout_add(60000, finish)
Gtk.main()
print("RESULT mode=%s cycles=%d crash=%s last=%s" % (MODE, CYCLES, state["crash"], state["title"]), flush=True)

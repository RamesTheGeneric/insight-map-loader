#!/usr/bin/env python3
"""visualize3d -- interactive 3D viewer for a decoded Insight map.

Writes a single self-contained HTML file (hand-rolled WebGL2, no libraries, no
network) that renders the map's points, the headset trajectory reconstructed
from the keyrig poses, and a metric floor grid.

    ./visualize3d.py /tmp/mapdb108 --out map3d.html

Two maps can be shown together to inspect an alignment, with B optionally
transformed by a solved 4-DoF transform:

    ./visualize3d.py /tmp/mapdb108 --dump2 /tmp/mapdb132 \
        --yaw 358.2 --t 0.31 0.0 -1.15 --out align3d.html

Controls: drag orbit, right-drag or shift-drag pan, wheel zoom, R reset.

The trajectory needs one convention that is not obvious: a keyrig pose is
world->camera, so the camera CENTRE is -R^T t, not t. Read literally, t puts
the headset through a 4.2 m vertical range; as -R^T t it walks a 1.2 m band
across a 4.5 m room, which is a person.
"""
import argparse
import json
import os

import numpy as np

import mapdata as md

PALETTE = ["#4fc3f7", "#ff8a65", "#aed581", "#ba68c8", "#ffd54f",
           "#4db6ac", "#f06292", "#90a4ae"]


def keyrig_centres(mapdb: str, node) -> np.ndarray:
    """Headset positions for one node's keyrigs, in the ROOT frame."""
    rec = md.load(os.path.join(mapdb, f"nd_{node.node}_{md.KIND_GRAPH}.mapdata"))
    poses = [np.asarray(k[2], float) for k in rec.get(6, []) if 2 in k]
    if not poses:
        return np.zeros((0, 3))
    C = np.array([-md.quat_to_R(p[:4]).T @ p[4:7] for p in poses])
    return C @ md.quat_to_R(node.pose[:4]).T + node.pose[4:7]


def apply_4dof(P: np.ndarray, yaw_deg: float, t) -> np.ndarray:
    c, s = np.cos(np.radians(yaw_deg)), np.sin(np.radians(yaw_deg))
    R = np.array([[c, 0, s], [0, 1, 0], [-s, 0, c]])
    return P @ R.T + np.asarray(t, float)


def collect(mapdb: str, max_range: float, yaw=0.0, t=(0, 0, 0), tag=0):
    m = md.Map(mapdb)
    groups, pts, node_id, trails = [], [], [], []
    for i, n in enumerate(m.nodes):
        P = n.points(max_range=max_range)
        C = keyrig_centres(mapdb, n)
        if yaw or any(t):
            P = apply_4dof(P, yaw, t)
            C = apply_4dof(C, yaw, t) if len(C) else C
        groups.append({"id": n.node[:8], "n": len(P), "trail": len(C), "map": tag})
        pts.append(P)
        node_id.append(np.full(len(P), len(groups) - 1))
        trails.append(C)
    P = np.vstack(pts) if pts else np.zeros((0, 3))
    return {
        "groups": groups,
        "pts": np.round(P, 4).ravel().tolist(),
        "node": np.concatenate(node_id).tolist() if node_id else [],
        "trails": [np.round(c, 4).ravel().tolist() for c in trails],
        "root": m.nodes[0].root if m.nodes else "",
    }


HTML = r"""<!doctype html><html><head><meta charset="utf-8">
<title>__TITLE__</title>
<style>
 :root{--bg:#0b0d10;--panel:#141820;--line:#232a35;--fg:#c9d1d9;--dim:#7d8590;--acc:#4fc3f7}
 *{box-sizing:border-box}
 html,body{margin:0;height:100%;overflow:hidden;background:var(--bg);color:var(--fg);
   font:13px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}
 canvas{display:block;width:100vw;height:100vh;cursor:grab}
 canvas.drag{cursor:grabbing}
 #ui{position:fixed;top:14px;left:14px;width:250px;background:rgba(20,24,32,.93);
   border:1px solid var(--line);border-radius:10px;padding:14px;backdrop-filter:blur(8px);
   max-height:calc(100vh - 28px);overflow:auto}
 h1{margin:0 0 2px;font-size:14px;font-weight:600;letter-spacing:.01em}
 .sub{color:var(--dim);font-size:11px;margin-bottom:12px;font-variant-numeric:tabular-nums}
 .grp{border-top:1px solid var(--line);padding-top:10px;margin-top:10px}
 .lbl{color:var(--dim);font-size:10px;text-transform:uppercase;letter-spacing:.09em;margin-bottom:6px}
 .row{display:flex;align-items:center;gap:8px;margin:4px 0}
 .row label{flex:1;display:flex;align-items:center;gap:7px;cursor:pointer;min-width:0}
 .sw{width:9px;height:9px;border-radius:50%;flex:none}
 .nm{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:11px;
   overflow:hidden;text-overflow:ellipsis}
 .ct{color:var(--dim);font-size:10px;font-variant-numeric:tabular-nums;flex:none}
 input[type=range]{width:100%;accent-color:var(--acc)}
 input[type=checkbox]{accent-color:var(--acc);margin:0;flex:none}
 .seg{display:flex;gap:4px}
 .seg button{flex:1;background:#1b212b;color:var(--dim);border:1px solid var(--line);
   border-radius:6px;padding:5px 0;font:inherit;font-size:11px;cursor:pointer}
 .seg button.on{background:var(--acc);color:#06202b;border-color:var(--acc);font-weight:600}
 .seg button:focus-visible,.row label:focus-within{outline:2px solid var(--acc);outline-offset:2px}
 kbd{background:#1b212b;border:1px solid var(--line);border-radius:4px;padding:1px 5px;font-size:10px}
 #hint{position:fixed;bottom:14px;left:50%;transform:translateX(-50%);color:var(--dim);
   font-size:11px;background:rgba(20,24,32,.85);border:1px solid var(--line);
   border-radius:8px;padding:6px 12px}
 #scale{position:fixed;bottom:14px;right:14px;color:var(--dim);font-size:11px;
   background:rgba(20,24,32,.85);border:1px solid var(--line);border-radius:8px;padding:6px 12px;
   font-variant-numeric:tabular-nums}
</style></head><body>
<canvas id="c"></canvas>
<div id="ui">
  <h1>__HEADING__</h1>
  <div class="sub" id="stats"></div>
  <div class="grp"><div class="lbl">Colour</div>
    <div class="seg" id="mode">
      <button data-m="0" class="on">Node</button>
      <button data-m="1">Height</button>
      <button data-m="2">Map</button>
    </div></div>
  <div class="grp"><div class="lbl">Point size</div>
    <input type="range" id="size" min="0.5" max="12" step="0.5" value="3"></div>
  <div class="grp"><div class="lbl">Show</div>
    <div class="row"><label><input type="checkbox" id="trail" checked>
      <span class="nm">keyrig viewpoints</span></label></div>
    <div class="row"><label><input type="checkbox" id="grid" checked>
      <span class="nm">floor grid (1 m)</span></label></div>
  </div>
  <div class="grp"><div class="lbl">Nodes</div><div id="nodes"></div></div>
</div>
<div id="hint">drag orbit &middot; shift-drag pan &middot; wheel zoom &middot; <kbd>R</kbd> reset</div>
<div id="scale"></div>
<script>
const DATA = __DATA__;
const PAL = __PAL__;

/* ---------- tiny mat4 ---------- */
const M4={
 mul:(a,b)=>{const o=new Float32Array(16);for(let i=0;i<4;i++)for(let j=0;j<4;j++){let s=0;
   for(let k=0;k<4;k++)s+=a[k*4+j]*b[i*4+k];o[i*4+j]=s;}return o;},
 persp:(f,ar,n,fa)=>{const t=1/Math.tan(f/2);return new Float32Array(
   [t/ar,0,0,0, 0,t,0,0, 0,0,(fa+n)/(n-fa),-1, 0,0,2*fa*n/(n-fa),0]);},
 look:(e,c,u)=>{const z=norm(sub(e,c)),x=norm(cross(u,z)),y=cross(z,x);
   return new Float32Array([x[0],y[0],z[0],0, x[1],y[1],z[1],0, x[2],y[2],z[2],0,
     -dot(x,e),-dot(y,e),-dot(z,e),1]);}};
const sub=(a,b)=>[a[0]-b[0],a[1]-b[1],a[2]-b[2]];
const cross=(a,b)=>[a[1]*b[2]-a[2]*b[1],a[2]*b[0]-a[0]*b[2],a[0]*b[1]-a[1]*b[0]];
const dot=(a,b)=>a[0]*b[0]+a[1]*b[1]+a[2]*b[2];
const norm=a=>{const l=Math.hypot(...a)||1;return [a[0]/l,a[1]/l,a[2]/l];};

/* ---------- gl ---------- */
const cv=document.getElementById('c');
const gl=cv.getContext('webgl2',{antialias:true,alpha:false});
if(!gl){document.body.innerHTML='<p style="padding:24px">WebGL2 unavailable in this browser.</p>';}

const VS=`#version 300 es
in vec3 aPos; in float aNode; in float aMap;
uniform mat4 uMVP; uniform float uSize; uniform float uPxScale; uniform int uMode;
uniform float uVis[32]; uniform vec3 uPal[32]; uniform vec2 uYRange;
out vec3 vCol; out float vDrop;
vec3 ramp(float t){ // deep blue -> cyan -> amber: reads as height, not decoration
  t=clamp(t,0.0,1.0);
  vec3 a=vec3(0.16,0.24,0.52), b=vec3(0.20,0.72,0.78), c=vec3(0.99,0.79,0.32);
  return t<0.5? mix(a,b,t*2.0) : mix(b,c,(t-0.5)*2.0);}
void main(){
  int n=int(aNode);
  vDrop = uVis[n] < 0.5 ? 1.0 : 0.0;
  gl_Position = uMVP*vec4(aPos,1.0);
  // perspective-correct, but resolution-independent: uSize is the on-screen
  // diameter in px at 1 m. Scaled by canvas height so a HiDPI buffer does not
  // silently double it.
  gl_PointSize = clamp(uSize*uPxScale/max(gl_Position.w,0.05),1.0,26.0);
  if(uMode==0) vCol=uPal[n];
  else if(uMode==1) vCol=ramp((aPos.y-uYRange.x)/max(uYRange.y-uYRange.x,0.001));
  else vCol = aMap<0.5? vec3(0.31,0.76,0.97) : vec3(1.00,0.54,0.40);
}`;
const FS=`#version 300 es
precision mediump float;
in vec3 vCol; in float vDrop; out vec4 o;
void main(){ if(vDrop>0.5) discard;
  vec2 d=gl_PointCoord-0.5; float r=dot(d,d);
  if(r>0.25) discard;
  o=vec4(vCol*(1.0-r*0.7), 1.0);}`;
const LVS=`#version 300 es
in vec3 aPos; uniform mat4 uMVP; uniform vec3 uCol; uniform float uPt; out vec3 vCol;
void main(){gl_Position=uMVP*vec4(aPos,1.0); gl_PointSize=uPt; vCol=uCol;}`;
const LFS=`#version 300 es
precision mediump float; in vec3 vCol; uniform float uA; out vec4 o;
void main(){o=vec4(vCol,uA);}`;

function prog(vs,fs){
  const p=gl.createProgram();
  for(const [t,s] of [[gl.VERTEX_SHADER,vs],[gl.FRAGMENT_SHADER,fs]]){
    const sh=gl.createShader(t); gl.shaderSource(sh,s); gl.compileShader(sh);
    if(!gl.getShaderParameter(sh,gl.COMPILE_STATUS)) console.error(gl.getShaderInfoLog(sh));
    gl.attachShader(p,sh);}
  gl.linkProgram(p); return p;}
const P=prog(VS,FS), LP=prog(LVS,LFS);

function buf(arr,loc,size,p){
  const b=gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER,b);
  gl.bufferData(gl.ARRAY_BUFFER,new Float32Array(arr),gl.STATIC_DRAW);
  const l=gl.getAttribLocation(p,loc);
  if(l>=0){gl.enableVertexAttribArray(l); gl.vertexAttribPointer(l,size,gl.FLOAT,false,0,0);} return b;}

/* ---------- geometry ---------- */
const pts=DATA.pts, nodeIdx=DATA.node;
const NP=pts.length/3;
const mapOf=nodeIdx.map(i=>DATA.groups[i].map);
// Framing must be ROBUST. A handful of far-triangulated points sit tens of
// metres out; centring on the mean and sizing to the max would frame the
// outliers and squash the room into a blob. Median centre, 95th-percentile
// radius.
const pct=(a,p)=>{const s=Float64Array.from(a).sort(); return s[Math.min(s.length-1,
  Math.max(0,Math.floor(p*(s.length-1))))]||0;};
const col=k=>Array.from({length:NP},(_,i)=>pts[i*3+k]);
const cx=pct(col(0),0.5), cy=pct(col(1),0.5), cz=pct(col(2),0.5);
const ymin=pct(col(1),0.01), ymax=pct(col(1),0.99);
const dists=Array.from({length:NP},(_,i)=>
  Math.hypot(pts[i*3]-cx,pts[i*3+1]-cy,pts[i*3+2]-cz));
const rad=pct(dists,0.95)||4;

const vaoP=gl.createVertexArray(); gl.bindVertexArray(vaoP);
buf(pts,'aPos',3,P); buf(nodeIdx,'aNode',1,P); buf(mapOf,'aMap',1,P);

// floor grid, one line per metre, sized to the cloud
const gy=ymin-0.05, R=Math.ceil(rad)+1, gridV=[];
for(let i=-R;i<=R;i++){
  gridV.push(cx-R,gy,cz+i, cx+R,gy,cz+i, cx+i,gy,cz-R, cx+i,gy,cz+R);}
const vaoG=gl.createVertexArray(); gl.bindVertexArray(vaoG); buf(gridV,'aPos',3,LP);

// headset trajectories, one line strip per node
const trailV=[],trailRange=[];
DATA.trails.forEach(t=>{trailRange.push([trailV.length/3,t.length/3]);
  for(const v of t) trailV.push(v);});
const vaoT=gl.createVertexArray(); gl.bindVertexArray(vaoT);
if(trailV.length) buf(trailV,'aPos',3,LP);

/* ---------- camera ---------- */
let az=0.7, el=0.45, dist=rad*2.4, tgt=[cx,cy,cz];
function reset(){az=0.7;el=0.45;dist=rad*2.4;tgt=[cx,cy,cz];draw();}
function eye(){return [tgt[0]+dist*Math.cos(el)*Math.sin(az),
                       tgt[1]+dist*Math.sin(el),
                       tgt[2]+dist*Math.cos(el)*Math.cos(az)];}
let drag=null;
cv.addEventListener('pointerdown',e=>{drag={x:e.clientX,y:e.clientY,
  pan:e.shiftKey||e.button===2}; cv.setPointerCapture(e.pointerId); cv.classList.add('drag');});
cv.addEventListener('pointerup',e=>{drag=null;cv.classList.remove('drag');});
cv.addEventListener('contextmenu',e=>e.preventDefault());
cv.addEventListener('pointermove',e=>{
  if(!drag)return; const dx=e.clientX-drag.x, dy=e.clientY-drag.y;
  drag.x=e.clientX; drag.y=e.clientY;
  if(drag.pan){
    const ev=eye(), f=norm(sub(tgt,ev)), r=norm(cross(f,[0,1,0])), u=cross(r,f);
    const k=dist*0.0016;
    for(let i=0;i<3;i++) tgt[i]+= -r[i]*dx*k + u[i]*dy*k;
  }else{ az-=dx*0.006; el=Math.max(-1.5,Math.min(1.5,el+dy*0.006)); }
  draw();});
cv.addEventListener('wheel',e=>{e.preventDefault();
  dist=Math.max(0.4,Math.min(rad*14,dist*Math.exp(e.deltaY*0.0012))); draw();},{passive:false});
addEventListener('keydown',e=>{if(e.key==='r'||e.key==='R')reset();});

/* ---------- ui ---------- */
let mode=0, psize=3, showTrail=true, showGrid=true;
const vis=DATA.groups.map(()=>1);
const nodesEl=document.getElementById('nodes');
DATA.groups.forEach((g,i)=>{
  const row=document.createElement('div'); row.className='row';
  row.innerHTML=`<label><input type="checkbox" checked data-i="${i}">
    <span class="sw" style="background:${PAL[i%PAL.length]}"></span>
    <span class="nm">${g.id}</span></label><span class="ct">${g.n}</span>`;
  nodesEl.appendChild(row);});
nodesEl.addEventListener('change',e=>{
  const i=+e.target.dataset.i; vis[i]=e.target.checked?1:0; draw();});
document.getElementById('mode').addEventListener('click',e=>{
  const b=e.target.closest('button'); if(!b)return;
  mode=+b.dataset.m;
  [...e.currentTarget.children].forEach(x=>x.classList.toggle('on',x===b)); draw();});
document.getElementById('size').addEventListener('input',e=>{psize=+e.target.value;draw();});
document.getElementById('trail').addEventListener('change',e=>{showTrail=e.target.checked;draw();});
document.getElementById('grid').addEventListener('change',e=>{showGrid=e.target.checked;draw();});

const nTrail=DATA.trails.reduce((a,t)=>a+t.length/3,0);
document.getElementById('stats').textContent =
  `${NP.toLocaleString()} points · ${DATA.groups.length} nodes · ${nTrail} keyrigs`;

/* ---------- draw ---------- */
const palFlat=[]; for(let i=0;i<32;i++){const h=PAL[i%PAL.length];
  palFlat.push(parseInt(h.slice(1,3),16)/255,parseInt(h.slice(3,5),16)/255,parseInt(h.slice(5,7),16)/255);}

function draw(){
  const dpr=Math.min(devicePixelRatio||1,2);
  const w=Math.floor(innerWidth*dpr), h=Math.floor(innerHeight*dpr);
  if(cv.width!==w||cv.height!==h){cv.width=w;cv.height=h;}
  gl.viewport(0,0,w,h);
  gl.clearColor(0.043,0.051,0.063,1); gl.clear(gl.COLOR_BUFFER_BIT|gl.DEPTH_BUFFER_BIT);
  gl.enable(gl.DEPTH_TEST);
  const ev=eye();
  const mvp=M4.mul(M4.persp(1.0,w/h,0.05,Math.max(200,rad*40)),M4.look(ev,tgt,[0,1,0]));

  gl.enable(gl.BLEND); gl.blendFunc(gl.SRC_ALPHA,gl.ONE_MINUS_SRC_ALPHA);
  gl.useProgram(LP);
  gl.uniformMatrix4fv(gl.getUniformLocation(LP,'uMVP'),false,mvp);
  if(showGrid){
    gl.bindVertexArray(vaoG);
    gl.uniform3f(gl.getUniformLocation(LP,'uCol'),0.19,0.22,0.28);
    gl.uniform1f(gl.getUniformLocation(LP,'uA'),0.85);
    gl.uniform1f(gl.getUniformLocation(LP,'uPt'),1.0);
    gl.drawArrays(gl.LINES,0,gridV.length/3);}
  // Keyrigs are drawn as square MARKERS, not a connected path. They carry no
  // timestamp, so joining them into a line strip would invent a walking order
  // that the data does not contain -- and it renders as obvious spaghetti.
  if(showTrail&&trailV.length){
    gl.bindVertexArray(vaoT);
    gl.uniform1f(gl.getUniformLocation(LP,'uA'),0.95);
    gl.uniform1f(gl.getUniformLocation(LP,'uPt'),7.0);
    trailRange.forEach(([off,cnt],i)=>{
      if(!vis[i]||!cnt)return;
      const h=PAL[i%PAL.length];
      gl.uniform3f(gl.getUniformLocation(LP,'uCol'),
        parseInt(h.slice(1,3),16)/255,parseInt(h.slice(3,5),16)/255,parseInt(h.slice(5,7),16)/255);
      gl.drawArrays(gl.POINTS,off,cnt);});}
  gl.disable(gl.BLEND);

  gl.useProgram(P); gl.bindVertexArray(vaoP);
  gl.uniformMatrix4fv(gl.getUniformLocation(P,'uMVP'),false,mvp);
  gl.uniform1f(gl.getUniformLocation(P,'uSize'),psize);
  gl.uniform1f(gl.getUniformLocation(P,'uPxScale'),h/64.0);
  gl.uniform1i(gl.getUniformLocation(P,'uMode'),mode);
  gl.uniform2f(gl.getUniformLocation(P,'uYRange'),ymin,ymax);
  gl.uniform1fv(gl.getUniformLocation(P,'uVis'),new Float32Array(
    Array.from({length:32},(_,i)=>vis[i]===undefined?0:vis[i])));
  gl.uniform3fv(gl.getUniformLocation(P,'uPal'),new Float32Array(palFlat));
  gl.drawArrays(gl.POINTS,0,NP);

  document.getElementById('scale').textContent =
    `${dist.toFixed(1)} m out · cloud ${(2*rad).toFixed(1)} m across`;
}
addEventListener('resize',draw);
reset();
</script></body></html>"""


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mapdb")
    ap.add_argument("--dump2", help="a second mapdb to overlay")
    ap.add_argument("--yaw", type=float, default=0.0, help="yaw (deg) applied to map B")
    ap.add_argument("--t", type=float, nargs=3, default=[0, 0, 0], help="translation for map B")
    ap.add_argument("--max-range", type=float, default=8.0)
    ap.add_argument("--out", default="insight_map3d.html")
    args = ap.parse_args()

    A = collect(args.mapdb, args.max_range, tag=0)
    name = os.path.basename(args.mapdb.rstrip("/")) or "map"
    if args.dump2:
        B = collect(args.dump2, args.max_range, args.yaw, args.t, tag=1)
        off = len(A["groups"])
        A["groups"] += B["groups"]
        A["pts"] += B["pts"]
        A["node"] += [i + off for i in B["node"]]
        A["trails"] += B["trails"]
        name += " + " + os.path.basename(args.dump2.rstrip("/"))

    heading = "Insight map"
    doc = (HTML.replace("__DATA__", json.dumps(A))
               .replace("__PAL__", json.dumps(PALETTE))
               .replace("__TITLE__", f"Insight map · {name}")
               .replace("__HEADING__", heading))
    with open(args.out, "w") as f:
        f.write(doc)
    npts = len(A["pts"]) // 3
    print(f"wrote {args.out}  ({npts} points, {len(A['groups'])} nodes, "
          f"{os.path.getsize(args.out)/1024:.0f} KB)")


if __name__ == "__main__":
    raise SystemExit(main())

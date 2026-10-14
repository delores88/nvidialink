/**
 * The pairing QR, drawn by PhaseForge (Amendment A1 gives `qr_url`): error correction level H, rounded dot
 * modules, rounded-square finder "eyes", a Spark mark in a cleared centre (level H restores up to 30% of the code,
 * the mark clears about 4%), and a 4-module quiet zone. Modules are filled with an iridescent gradient whose every
 * colour keeps at least 7:1 contrast against the white tile, in both themes. Encoding: qrcode-generator (MIT).
 * Geometry is in module units; the component scales it. Nothing here touches the DOM.
 */
import qrcode from './qrcode.mjs';

export const QR_TEXT_MAX=1024,QR_LEVEL='H',QR_MARGIN=4,DOT_RADIUS=.45;
const QR_URL=/^https?:\/\/[\x21-\x7e]+$/;
/** The phone opens a page served by the Spark: http(s), printable ASCII, short enough to scan. */
export const isQrUrl=value=>typeof value==='string'&&value.length<=QR_TEXT_MAX&&QR_URL.test(value);

export const TILE='#ffffff';
/** Deep blue → violet → teal. Each stop, and every sRGB mix between neighbours, stays ≥ 7:1 on the tile. */
export const IRIDESCENT=Object.freeze([{offset:0,color:'#1e3a8a'},{offset:.5,color:'#4c1d95'},{offset:1,color:'#134e4a'}]);
export const CHECK_COLOR='#15803d';

const channel=value=>{const c=value/255;return c<=.04045?c/12.92:((c+.055)/1.055)**2.4;};
const rgb=hex=>{const m=/^#?([0-9a-f]{6})$/i.exec(hex);if(!m)throw Error(`Not a hex colour: ${hex}`);const n=parseInt(m[1],16);return [n>>16&255,n>>8&255,n&255];};
/** WCAG relative luminance and contrast ratio. */
export function luminance(hex){const [r,g,b]=rgb(hex);return .2126*channel(r)+.7152*channel(g)+.0722*channel(b);}
export function contrastRatio(a,b){const [hi,lo]=[luminance(a),luminance(b)].sort((x,y)=>y-x);return (hi+.05)/(lo+.05);}
/** sRGB mix, as SVG gradients interpolate by default. */
export function mixHex(a,b,t){const x=rgb(a),y=rgb(b);return '#'+x.map((v,i)=>Math.round(v+(y[i]-v)*t).toString(16).padStart(2,'0')).join('');}
/** The gradient colour at position 0..1 (used to tint particles so they match the settled code). */
export function iridescentAt(t){
  const p=Math.min(1,Math.max(0,t));
  for(let i=1;i<IRIDESCENT.length;i++){const a=IRIDESCENT[i-1],b=IRIDESCENT[i];if(p<=b.offset)return mixHex(a.color,b.color,(p-a.offset)/(b.offset-a.offset||1));}
  return IRIDESCENT.at(-1).color;
}

const fixed=value=>Math.round(value*1000)/1000;
export function roundedRect(x,y,w,h,r){
  const q=Math.min(r,w/2,h/2),f=fixed;
  return `M${f(x+q)} ${f(y)}H${f(x+w-q)}A${f(q)} ${f(q)} 0 0 1 ${f(x+w)} ${f(y+q)}V${f(y+h-q)}A${f(q)} ${f(q)} 0 0 1 ${f(x+w-q)} ${f(y+h)}H${f(x+q)}A${f(q)} ${f(q)} 0 0 1 ${f(x)} ${f(y+h-q)}V${f(y+q)}A${f(q)} ${f(q)} 0 0 1 ${f(x+q)} ${f(y)}Z`;
}
// Alignment pattern centres per version (ISO/IEC 18004 Annex E). Drawn solid like the eyes: as separate dots their
// 1:1:1 runs break up at high resolution and decoders lose the code.
export const ALIGNMENT=Object.freeze([null,[],[6,18],[6,22],[6,26],[6,30],[6,34],[6,22,38],[6,24,42],[6,26,46],[6,28,50],[6,30,54],[6,32,58],[6,34,62],[6,26,46,66],
  [6,26,48,70],[6,26,50,74],[6,30,54,78],[6,30,56,82],[6,30,58,86],[6,34,62,90],[6,28,50,72,94],[6,26,50,74,98],[6,30,54,78,102],[6,28,54,80,106],[6,32,58,84,110],
  [6,30,58,86,114],[6,34,62,90,118],[6,26,50,74,98,122],[6,30,54,78,102,126],[6,26,52,78,104,130],[6,30,56,82,108,134],[6,34,60,86,112,138],[6,30,58,86,114,142],
  [6,34,62,90,118,146],[6,30,54,78,102,126,150],[6,24,50,76,102,128,154],[6,28,54,80,106,132,158],[6,32,58,84,110,136,162],[6,26,54,82,110,138,166],[6,30,58,86,114,142,170]]);
/** Alignment pattern top-left module positions for an n-module code (those clear of the three finders). */
export function alignmentOrigins(n){
  const version=(n-17)/4,centres=ALIGNMENT[version]||[],out=[];
  for(const r of centres)for(const c of centres){if((r<9&&c<9)||(r<9&&c>n-10)||(r>n-10&&c<9))continue;out.push([r-2,c-2]);}
  return out;
}
/** An alignment pattern at module (x,y): a rounded 5×5 ring (even-odd) and a rounded centre module. */
export const alignRing=(x,y)=>roundedRect(x,y,5,5,1.6)+roundedRect(x+1,y+1,3,3,1);
export const alignDot=(x,y)=>roundedRect(x+2,y+2,1,1,.4);
/** A finder eye at module (x,y): a rounded 7×7 ring (even-odd) and a rounded 3×3 pupil. */
export const eyeRing=(x,y)=>roundedRect(x,y,7,7,2.3)+roundedRect(x+1,y+1,5,5,1.6);
export const eyePupil=(x,y)=>roundedRect(x+2,y+2,3,3,1.1);
/** The Spark mark: a four-point sparkle. */
export function sparkPath(cx,cy,r){const f=fixed;return `M${f(cx)} ${f(cy-r)}Q${f(cx)} ${f(cy)} ${f(cx+r)} ${f(cy)}Q${f(cx)} ${f(cy)} ${f(cx)} ${f(cy+r)}Q${f(cx)} ${f(cy)} ${f(cx-r)} ${f(cy)}Q${f(cx)} ${f(cy)} ${f(cx)} ${f(cy-r)}Z`;}
/** A rounded-rectangle ring that starts at 12 o'clock and runs clockwise (for the countdown stroke). */
export function ringPath(w,h,r,inset){
  const x=inset,y=inset,W=w-2*inset,H=h-2*inset,q=Math.min(r,W/2,H/2),f=fixed;
  return `M${f(x+W/2)} ${f(y)}H${f(x+W-q)}A${f(q)} ${f(q)} 0 0 1 ${f(x+W)} ${f(y+q)}V${f(y+H-q)}A${f(q)} ${f(q)} 0 0 1 ${f(x+W-q)} ${f(y+H)}H${f(x+q)}A${f(q)} ${f(q)} 0 0 1 ${f(x)} ${f(y+H-q)}V${f(y+q)}A${f(q)} ${f(q)} 0 0 1 ${f(x+q)} ${f(y)}Z`;
}
/** The approval check as a polyline in tile fractions, and evenly spaced points along it (particle targets). */
export const CHECK=Object.freeze([[.3,.52],[.45,.67],[.72,.37]]);
export function checkPoints(size,count=96){
  const pts=CHECK.map(([x,y])=>[x*size,y*size]),lengths=pts.slice(1).map((p,i)=>Math.hypot(p[0]-pts[i][0],p[1]-pts[i][1])),total=lengths.reduce((a,b)=>a+b,0),out=[];
  for(let k=0;k<count;k++){
    let d=total*k/(count-1),i=0;while(i<lengths.length-1&&d>lengths[i]){d-=lengths[i];i++;}
    const t=lengths[i]?Math.min(1,d/lengths[i]):0;out.push({x:pts[i][0]+(pts[i+1][0]-pts[i][0])*t,y:pts[i][1]+(pts[i+1][1]-pts[i][1])*t});
  }
  return out;
}

/**
 * {text, size, modules, margin, dots, eyes, logo, cells}. `dots` are the drawn round modules (not in an eye or the
 * centre); `cells` are every drawn dark module (dots plus eye modules), one particle each; `logo` is the cleared
 * centre square. Coordinates include the quiet zone, so the tile spans 0..size.
 */
export function qrArt(text,{margin=QR_MARGIN}={}){
  if(!isQrUrl(text))return null;
  let code;
  try{code=qrcode(0,QR_LEVEL);code.addData(text,'Byte');code.make();}catch{return null;}
  const n=code.getModuleCount(),size=n+margin*2,mid=size/2;
  const finders=[[0,0],[0,n-7],[n-7,0]];
  const inFinder=(r,c)=>finders.some(([fr,fc])=>r>=fr&&r<fr+7&&c>=fc&&c<fc+7);
  // An odd square of about a fifth of the side, at least 5 modules, centred.
  let k=Math.max(5,Math.round(n*.2));if(k%2===0)k++;
  const lo=(n-k)/2,inLogo=(r,c)=>r>=lo&&r<lo+k&&c>=lo&&c<lo+k;
  // Alignment patterns that the centre mark covers stay cleared with it.
  const aligns=alignmentOrigins(n).filter(([r,c])=>!(r+5>lo&&r<lo+k&&c+5>lo&&c<lo+k));
  const inAlign=(r,c)=>aligns.some(([ar,ac])=>r>=ar&&r<ar+5&&c>=ac&&c<ac+5);
  const dots=[],cells=[],far=Math.hypot(mid-margin,mid-margin);
  for(let r=0;r<n;r++)for(let c=0;c<n;c++){
    if(!code.isDark(r,c)||inLogo(r,c))continue;
    const x=c+margin+.5,y=r+margin+.5,cell={x,y,d:Math.min(1,Math.hypot(x-mid,y-mid)/far)};
    cells.push(cell);if(!inFinder(r,c)&&!inAlign(r,c))dots.push(cell);
  }
  return {text,size,modules:n,margin,dots,cells,eyes:finders.map(([r,c])=>({x:c+margin,y:r+margin})),aligns:aligns.map(([r,c])=>({x:c+margin,y:r+margin})),
    logo:{x:lo+margin,y:lo+margin,size:k},level:QR_LEVEL};
}

/** Motion per state. Reduced motion: no particles, sweep, tilt or ambient particles; crossfades only. */
export function qrMotion({reduced=false,state='waiting'}={}){
  const full=!reduced;
  return {particles:full&&['starting','waiting','approved','denied'].includes(state),tilt:full&&state==='waiting',sweep:full&&state==='waiting',
    ambient:full&&state==='waiting',drift:full&&state==='waiting',crossfade:true};
}
/**
 * Where the ambient particles sit in the art box (CSS px, box×box, a square tile of side `tile` centred): around
 * the tile, always outside it (and so outside the quiet zone) even at the far end of their drift, and inside the box.
 */
export function ambientSpots(count,box,tile,drift){
  const c=box/2,spots=[];
  for(let i=0;i<count;i++){
    const a=(i/count)*Math.PI*2+.4,edge=Math.max(Math.abs(Math.cos(a)),Math.abs(Math.sin(a)));
    const inner=(tile/2+drift+4)/edge,outer=(c-drift-2)/edge,r=inner+(((i*37)%11)/10)*Math.max(0,outer-inner);
    spots.push({x:fixed(c+Math.cos(a)*r),y:fixed(c+Math.sin(a)*r)});
  }
  return spots;
}

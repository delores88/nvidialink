import {startVisibleRenderLoop} from './visible-render-loop.mjs';

// Connection decoration, not traffic or job telemetry. Bounded particles/backing store; no React updates per frame.
const COLORS=['#65d9ce','#74acff','#b798f8','#de9cd3'];
const LANES=[-1,-.66,-.33,0,.33,.66,1];
export function ribbonY(t,lane,height=100){
  return height*(.5+Math.sin(Math.PI*t)*(lane*.21+Math.sin(t*Math.PI*2+lane)*.07));
}
export const CONNECTION_RIBBONS=LANES.map(lane=>Array.from({length:49},(_,i)=>{
  const t=i/48;return `${i?'L':'M'}${(t*1000).toFixed(2)} ${ribbonY(t,lane).toFixed(2)}`;
}).join(' '));

export function startSparkParticles(canvas,environment=globalThis){
  const ctx=canvas?.getContext?.('2d');
  if(!ctx||!environment.requestAnimationFrame||!environment.cancelAnimationFrame)return ()=>{};
  let width=0,height=0,particles=[],last=-Infinity,disposed=false;
  const measure=()=>{
    if(disposed)return;
    width=Math.max(0,canvas.clientWidth||0);height=Math.max(0,canvas.clientHeight||0);
    const ratio=Math.min(1.5,Math.max(1,environment.devicePixelRatio||1));
    canvas.width=Math.min(2560,Math.round(width*ratio));canvas.height=Math.min(256,Math.round(height*ratio));
    if(!width||!height){particles=[];return;}
    ctx.setTransform(canvas.width/width,0,0,canvas.height/height,0,0);
    const count=Math.min(220,Math.max(72,Math.round(width/5)));
    particles=Array.from({length:count},(_,i)=>{
      const cluster=Math.floor(i/10),member=i%10;
      return {phase:(cluster*.381966+member*.006)%1,lane:LANES[cluster%LANES.length],
        direction:cluster%2?1:-1,speed:.035+(cluster%3)*.009,spread:Math.sin(i*2.399)*4,
        size:member===0?1.65:.65+(i%4)*.23,color:COLORS[cluster%COLORS.length]};
    });
    last=-Infinity;
  };
  measure();
  const loop=startVisibleRenderLoop(canvas,time=>{
    // Paint at most 30 fps, independent of monitor refresh rate; hidden/offscreen scheduling is stopped by the shared loop.
    if(time-last<1000/30||!width||!height)return;
    last=time;ctx.clearRect(0,0,width,height);
    for(const particle of particles){
      const t=((particle.phase+time/1000*particle.speed*particle.direction)%1+1)%1;
      const envelope=Math.sin(Math.PI*t),x=t*width,y=ribbonY(t,particle.lane,height)+particle.spread*envelope;
      const alpha=.12+envelope*.8;
      ctx.fillStyle=particle.color;
      // Soft, small halo without per-particle blur filters.
      ctx.globalAlpha=alpha*.12;ctx.beginPath();ctx.arc(x,y,particle.size*3.3,0,Math.PI*2);ctx.fill();
      ctx.globalAlpha=alpha;ctx.beginPath();ctx.arc(x,y,particle.size,0,Math.PI*2);ctx.fill();
      const tail=Math.max(0,Math.min(1,t-particle.direction*.007));
      ctx.globalAlpha=alpha*.36;ctx.strokeStyle=particle.color;ctx.lineWidth=.8;
      ctx.beginPath();ctx.moveTo(tail*width,ribbonY(tail,particle.lane,height)+particle.spread*envelope);ctx.lineTo(x,y);ctx.stroke();
    }
    ctx.globalAlpha=1;
  },{document:environment.document,requestFrame:environment.requestAnimationFrame.bind(environment),
    cancelFrame:environment.cancelAnimationFrame.bind(environment),IntersectionObserver:environment.IntersectionObserver});
  const observer=environment.ResizeObserver?new environment.ResizeObserver(measure):null;
  observer?.observe(canvas);
  if(!observer)environment.addEventListener?.('resize',measure);
  return ()=>{
    if(disposed)return;disposed=true;loop.dispose();observer?.disconnect();
    if(!observer)environment.removeEventListener?.('resize',measure);
    particles=[];canvas.width=0;canvas.height=0;
  };
}

// ---- 0.25: the state-driven filament (sparkLinkMotion.mjs decides what it shows) ----
// Same engine and budget as the ribbon above: bounded particles and backing store, at most 30 fps, paused
// whenever hidden or offscreen (startVisibleRenderLoop), and the canvas released when nothing moves.
const PACKET=24,DRIFT_MS=1600,GREY='#8a96a3';
function mix(hex,grey,amount){
  const p=h=>[1,3,5].map(i=>parseInt(h.slice(i,i+2),16));const a=p(hex),b=p(grey);
  return `rgb(${a.map((v,i)=>Math.round(v*amount+b[i]*(1-amount))).join(',')})`;
}
/** Deterministic per-particle layout (mulberry32), so a state replays the same filament. */
function prng(seed){let a=seed>>>0;return()=>{a=a+0x6D2B79F5>>>0;let t=a;t=Math.imul(t^t>>>15,t|1);t^=t+Math.imul(t^t>>>7,t|61);return((t^t>>>14)>>>0)/4294967296;};}

/** Create the filament's particles (pure; exported for tests). */
export function filamentParticles(count,seed=11){
  const r=prng(seed);
  return Array.from({length:count},(_,i)=>({t:r(),lane:LANES[i%LANES.length]*(.35+r()*.65),dir:i%3===2?-1:1,speed:.75+r()*.5,size:.6+r()*.9,
    color:COLORS[i%COLORS.length],vx:0,vy:0,dx:0,dy:0,alpha:1}));
}

/** One frame of the filament for `params` (pure geometry; exported for tests). Returns the drawn dots. */
export function filamentFrame(particles,params,{width,height,dt,snapAt=null,now=0,packet=null}){
  const dots=[],f=params.filament||{},sat=Number.isFinite(f.saturation)?f.saturation:1;
  const base=.03+.11*(f.speed||0);
  const drifting=!f.intact,since=snapAt===null?0:now-snapAt;
  for(const p of particles){
    if(drifting){
      // Snapped: each particle keeps flying outward from the break and fades.
      if(p.vx===0&&p.vy===0){const k=p.t<.5?-1:1;p.vx=k*(20+p.speed*30);p.vy=(p.lane||.1)*40;}
      p.dx+=p.vx*dt;p.dy+=p.vy*dt;p.vy+=18*dt;p.alpha=Math.max(0,1-since/DRIFT_MS);
    }else{
      p.t=((p.t+p.dir*base*p.speed*dt)%1+1)%1;
      // Re-knitting after a reconnect: drifted particles spring back onto the filament.
      const knit=Number.isFinite(f.knit)?f.knit:1;p.dx*=1-knit;p.dy*=1-knit;p.vx=0;p.vy=0;p.alpha=.35+.65*knit;
    }
    // A relay path is a dashed stream: particles in the gaps are not drawn.
    if(f.dashed&&!drifting&&((p.t*14)%1)>.58)continue;
    const env=Math.sin(Math.PI*p.t),x=p.t*width+p.dx,y=ribbonY(p.t,p.lane,height)+p.dy;
    const alpha=(.15+env*.8)*p.alpha;
    if(alpha<=.01)continue;
    dots.push({x,y,r:p.size,alpha,color:sat>=1?p.color:mix(p.color,GREY,sat)});
  }
  if(packet&&!drifting){
    // A compact packet at the job's real transfer position: up = toward the Spark, down = back.
    const at=packet.direction==='down'?1-packet.fill:packet.fill;
    for(let i=0;i<PACKET;i++){
      const a=i/PACKET*Math.PI*2,rad=2+(i%4);
      dots.push({x:Math.min(width,Math.max(0,at*width+Math.cos(a)*rad)),y:height/2+Math.sin(a)*rad*.7,r:1.1,alpha:packet.stalled?.45:.95,color:sat>=1?'#ffffff':mix('#ffffff',GREY,sat)});
    }
  }
  return dots;
}

/** Start the filament on `canvas`; `read()` returns the current linkMotion() parameters. Returns dispose. */
export function startLinkFilament(canvas,read,environment=globalThis){
  const ctx=canvas?.getContext?.('2d');
  if(!ctx||!environment.requestAnimationFrame||!environment.cancelAnimationFrame||typeof read!=='function')return ()=>{};
  let width=0,height=0,particles=[],last=-Infinity,disposed=false,snapAt=null,count=0,released=false;
  const measure=()=>{
    if(disposed)return;
    width=Math.max(0,canvas.clientWidth||0);height=Math.max(0,canvas.clientHeight||0);
    const ratio=Math.min(1.5,Math.max(1,environment.devicePixelRatio||1));
    canvas.width=Math.min(2560,Math.round(width*ratio));canvas.height=Math.min(256,Math.round(height*ratio));released=false;
    if(width&&height)ctx.setTransform(canvas.width/width,0,0,canvas.height/height,0,0);
    last=-Infinity;
  };
  measure();
  const loop=startVisibleRenderLoop(canvas,time=>{
    if(time-last<1000/30||!width||!height)return;
    const dt=Math.min(.1,last===-Infinity?0:(time-last)/1000);last=time;
    const params=read()||{};
    if(params.filament?.intact===false){if(snapAt===null)snapAt=time;}
    else if(snapAt!==null){
      // Reconnected: the drifted particles start scattered and re-knit onto the filament.
      snapAt=null;particles=filamentParticles(count);for(const p of particles){p.dx=(p.t-.5)*60;p.dy=p.lane*30;}
    }
    // Nothing moves (reduced motion, paused, or the snapped filament has drifted away): release the canvas.
    const idle=!params.animate||(snapAt!==null&&time-snapAt>DRIFT_MS&&!params.packet);
    if(idle){if(!released){ctx.clearRect(0,0,width,height);canvas.width=0;canvas.height=0;released=true;}return;}
    if(released)measure();
    const wanted=Math.max(0,Math.min(220,params.filament?.particles|0));
    if(wanted!==count){particles=filamentParticles(wanted);count=wanted;}
    ctx.clearRect(0,0,width,height);
    const dots=filamentFrame(particles,params,{width,height,dt,snapAt,now:time,packet:params.packet});
    for(const d of dots){
      ctx.fillStyle=d.color;
      ctx.globalAlpha=d.alpha*.12;ctx.beginPath();ctx.arc(d.x,d.y,d.r*3,0,Math.PI*2);ctx.fill();
      ctx.globalAlpha=d.alpha;ctx.beginPath();ctx.arc(d.x,d.y,d.r,0,Math.PI*2);ctx.fill();
    }
    ctx.globalAlpha=1;
  },{document:environment.document,requestFrame:environment.requestAnimationFrame.bind(environment),
    cancelFrame:environment.cancelAnimationFrame.bind(environment),IntersectionObserver:environment.IntersectionObserver});
  const observer=environment.ResizeObserver?new environment.ResizeObserver(measure):null;
  observer?.observe(canvas);
  if(!observer)environment.addEventListener?.('resize',measure);
  return ()=>{
    if(disposed)return;disposed=true;loop.dispose();observer?.disconnect();
    if(!observer)environment.removeEventListener?.('resize',measure);
    particles=[];canvas.width=0;canvas.height=0;
  };
}

/**
 * Particle choreography for the pairing QR (pure: no DOM, no clock; the component steps it from
 * requestAnimationFrame only while a particle phase runs). Units are QR modules; time is milliseconds.
 *
 *   swirl      a soft cloud orbiting the centre (at least SWIRL_MS, and for as long as no code has arrived)
 *   form       each particle springs onto one dark module, the centre first      → settled (the crisp SVG takes over)
 *   burst      approval: modules burst into particles that swirl briefly         → condense → checked
 *   scatter    denial: particles fly outward and fade                            → gone
 */
import {iridescentAt} from './sparkLinkQr.mjs';

export const SWIRL_MS=1000,FORM_STAGGER_MS=260,BURST_MS=420,SCATTER_MS=650,SETTLE_LIMIT_MS=2400;
const FORM={k:260,zeta:.68},CHECK={k:130,zeta:.72},SUBSTEP=8;
export const BUCKETS=16;
export const BUCKET_COLORS=Object.freeze(Array.from({length:BUCKETS},(_,i)=>iridescentAt(i/(BUCKETS-1))));
const ACTIVE=new Set(['swirl','form','burst','condense','scatter']);
export const isActive=sim=>!!sim&&ACTIVE.has(sim.phase);

/** Deterministic PRNG (mulberry32), so a seed replays the same cloud. */
export function random(seed){let a=seed>>>0;return()=>{a=a+0x6D2B79F5>>>0;let t=a;t=Math.imul(t^t>>>15,t|1);t^=t+Math.imul(t^t>>>7,t|61);return ((t^t>>>14)>>>0)/4294967296;};}

function spawn(sim){
  const r=sim.rand,mid=sim.size/2;
  return {angle:r()*Math.PI*2,radius:sim.size*(.1+r()*.3),omega:(1.1+r()*1.3)*(r()<.5?1:-1)*.9+.6,phase:r()*Math.PI*2,x:mid,y:mid,vx:0,vy:0,
    tx:null,ty:null,delay:0,bucket:Math.floor(r()*BUCKETS),alpha:0,fade:1};
}
export function createSim({size,count=240,seed=7}){
  const sim={size,mid:size/2,t:0,phase:'swirl',phaseAt:0,alpha:0,particles:[],targets:null,rand:random(seed)};
  for(let i=0;i<count;i++)sim.particles.push(spawn(sim));
  for(const p of sim.particles)orbit(sim,p,0);
  return sim;
}
function orbit(sim,p,dt){
  p.angle+=p.omega*dt;
  const breathe=Math.sin(sim.t/1000*.9+p.phase)*.6,r=p.radius+breathe,x=sim.mid+Math.cos(p.angle)*r,y=sim.mid+Math.sin(p.angle)*r*.92;
  if(dt>0){p.vx=(x-p.x)/dt;p.vy=(y-p.y)/dt;}p.x=x;p.y=y;
}
function spring(p,tx,ty,{k,zeta},dt){
  const c=2*zeta*Math.sqrt(k);p.vx+=(k*(tx-p.x)-c*p.vx)*dt;p.vy+=(k*(ty-p.y)-c*p.vy)*dt;p.x+=p.vx*dt;p.y+=p.vy*dt;
}
const near=(p,limit=.08,speed=.6)=>Math.abs(p.tx-p.x)<limit&&Math.abs(p.ty-p.y)<limit&&Math.hypot(p.vx,p.vy)<speed;
function snap(sim){for(const p of sim.particles){p.x=p.tx;p.y=p.ty;p.vx=0;p.vy=0;}}

/** One particle per drawn dark module; the centre modules fly in first; each takes its gradient colour. */
export function setTargets(sim,cells,size=sim.size){
  sim.size=size;sim.mid=size/2;
  while(sim.particles.length<cells.length){const p=spawn(sim);orbit(sim,p,0);sim.particles.push(p);}
  sim.particles.length=cells.length;
  cells.forEach((cell,i)=>{const p=sim.particles[i];p.tx=cell.x;p.ty=cell.y;p.delay=(cell.d??0)*FORM_STAGGER_MS;p.bucket=Math.round(Math.min(1,Math.max(0,(cell.x+cell.y)/(2*sim.size)))*(BUCKETS-1));});
  sim.targets=cells;
}
/** Particles placed on their modules (approval or denial after the code is already settled). */
export function placeOnCells(sim,cells){setTargets(sim,cells);snap(sim);sim.phase='settled';sim.alpha=1;sim.phaseAt=sim.t;}
export function burst(sim,points){
  const r=sim.rand;sim.checkTargets=points;sim.phase='burst';sim.phaseAt=sim.t;
  sim.particles.forEach((p,i)=>{const dx=p.x-sim.mid,dy=p.y-sim.mid,d=Math.hypot(dx,dy)||1,speed=6+r()*9;
    p.vx=dx/d*speed-dy/d*5;p.vy=dy/d*speed+dx/d*5;const target=points[i%points.length];p.tx=target.x+(r()-.5)*.5;p.ty=target.y+(r()-.5)*.5;});
}
export function scatter(sim){
  const r=sim.rand;sim.phase='scatter';sim.phaseAt=sim.t;
  for(const p of sim.particles){const dx=p.x-sim.mid,dy=p.y-sim.mid,d=Math.hypot(dx,dy)||1,speed=10+r()*14;p.vx=dx/d*speed+(r()-.5)*4;p.vy=dy/d*speed+(r()-.5)*4;p.fade=.8+r()*.4;}
}

/** Advances the choreography by dtMs (clamped to 50 ms per call) and returns the phase. */
export function step(sim,dtMs){
  let left=Math.min(50,Math.max(0,dtMs));
  while(left>0&&ACTIVE.has(sim.phase)){const ms=Math.min(SUBSTEP,left);left-=ms;tick(sim,ms);}
  return sim.phase;
}
function tick(sim,ms){
  const dt=ms/1000;sim.t+=ms;const since=sim.t-sim.phaseAt;
  if(sim.phase==='swirl'){
    sim.alpha=Math.min(1,sim.alpha+dt*3);for(const p of sim.particles)orbit(sim,p,dt);
    if(sim.targets&&sim.t>=SWIRL_MS){sim.phase='form';sim.phaseAt=sim.t;}
  }else if(sim.phase==='form'){
    sim.alpha=1;let done=true;
    for(const p of sim.particles){if(since<p.delay){orbit(sim,p,dt);done=false;continue;}
      // Leaving the orbit: keep a little of its momentum so the flight curves, not all of it (that only overshoots).
      if(!p.forming){p.forming=true;p.vx*=.25;p.vy*=.25;}
      spring(p,p.tx,p.ty,FORM,dt);if(!near(p,.1,.8))done=false;}
    if(done||since>SETTLE_LIMIT_MS){snap(sim);sim.phase='settled';sim.phaseAt=sim.t;}
  }else if(sim.phase==='burst'){
    const swirl=1-since/BURST_MS;
    for(const p of sim.particles){const dx=p.x-sim.mid,dy=p.y-sim.mid;p.vx+=(-dy*2.2*swirl-dx*.8)*dt;p.vy+=(dx*2.2*swirl-dy*.8)*dt;p.vx*=1-dt*2.5;p.vy*=1-dt*2.5;p.x+=p.vx*dt;p.y+=p.vy*dt;}
    if(since>=BURST_MS){sim.phase='condense';sim.phaseAt=sim.t;}
  }else if(sim.phase==='condense'){
    let done=true;for(const p of sim.particles){spring(p,p.tx,p.ty,CHECK,dt);if(!near(p,.12,.8))done=false;}
    if(done||since>SETTLE_LIMIT_MS){snap(sim);sim.phase='checked';sim.phaseAt=sim.t;}
  }else if(sim.phase==='scatter'){
    let visible=false;
    for(const p of sim.particles){p.vx*=1+dt*.8;p.vy*=1+dt*.8;p.x+=p.vx*dt;p.y+=p.vy*dt;p.alpha=Math.max(0,1-since/(SCATTER_MS*p.fade));if(p.alpha>0)visible=true;}
    sim.alpha=Math.max(0,1-since/SCATTER_MS);if(!visible){sim.phase='gone';sim.phaseAt=sim.t;}
  }
}

/** Draws the particles: one path per colour bucket (a few fills per frame, not one per particle). */
export function draw(ctx,sim,{scale,offset=0,dpr=1,width,height,radius=.42}){
  ctx.setTransform(dpr,0,0,dpr,0,0);ctx.clearRect(0,0,width,height);
  const perParticle=sim.phase==='scatter',r=radius*scale;
  for(let b=0;b<BUCKETS;b++){
    ctx.fillStyle=BUCKET_COLORS[b];
    if(perParticle){for(const p of sim.particles){if(p.bucket!==b||p.alpha<=0)continue;ctx.globalAlpha=p.alpha;ctx.beginPath();ctx.arc(offset+p.x*scale,offset+p.y*scale,r,0,Math.PI*2);ctx.fill();}continue;}
    ctx.globalAlpha=sim.alpha;ctx.beginPath();
    for(const p of sim.particles){if(p.bucket!==b)continue;const x=offset+p.x*scale,y=offset+p.y*scale;ctx.moveTo(x+r,y);ctx.arc(x,y,r,0,Math.PI*2);}
    ctx.fill();
  }
  ctx.globalAlpha=1;
}

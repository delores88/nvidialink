// Spark Link animation (0.25 "motion encodes real state"). Pure: no DOM, no clock. Everything the
// connection animation shows comes from here, computed from what PhaseForge measured:
//
//   setup         particles assemble the Spark glyph piece by piece as each install step really
//                 completes (connecting, copying, verifying, starting); Ready ignites it once.
//   idle          a living filament. Particle speed follows measured throughput, density follows
//                 latency; LAN is a steady stream, a relay is a dashed stream through a pulsing
//                 relay node. The Spark breathes with GPU utilization only when the node reports it.
//   job           a packet travels to the Spark with real upload progress; the Spark glows with the
//                 job's progress; results stream back into an output icon with download progress
//                 and settle softly when the atomic import completes.
//   paused        the filament freezes and turns grey.
//   disconnected  the filament snaps and its particles drift; "Spark unreachable, waiting to
//                 reconnect (elapsed)".
//   reconnected   the particles re-knit the filament; a reattached job's packet resumes from its
//                 real progress.
//   path change   (LAN -> relay or back) the filament morphs; the panel shows its one-line toast.
//
// Reduced motion: the same states, static (no canvas, no travelling particles), crossfaded by CSS.

export const MOTION_STATES=Object.freeze(['setup','idle','job','paused','disconnected','reconnected']);
/** Seconds after a reconnect during which the filament re-knits. */
export const REKNIT_S=2.4;
/** At most this many particles in the filament (2-3 ms per frame budget on the laptop). */
export const MAX_PARTICLES=180;
const SLOW_MBPS=10;

const clamp=(v,lo,hi)=>Math.min(hi,Math.max(lo,v));
const finite=v=>typeof v==='number'&&Number.isFinite(v);

/** The setup steps that assemble the Spark glyph (Ready ignites the assembled glyph). */
export const ASSEMBLY_STEPS=Object.freeze(['connecting','copying','verifying','starting']);

/** Pieces of the Spark glyph assembled for a setup step: a step's piece lands when the step completes. */
export function setupAssembly(step,failed=false){
  const index=ASSEMBLY_STEPS.indexOf(step),ready=step==='ready'&&!failed;
  const assembled=ready?ASSEMBLY_STEPS.length:Math.max(0,index);
  return {pieces:ASSEMBLY_STEPS.length,assembled,current:ready||failed?null:ASSEMBLY_STEPS[Math.max(0,index)]??null,ignite:ready,failed:!!failed,
    // Particles stream from the laptop only while a step is running.
    streaming:!ready&&!failed};
}

/** Particle speed (0..1) from measured throughput (MB/s): log scale, 2 MB/s -> ~0.21, 17 -> ~0.56, 110 -> ~0.91. */
export function speedFor(mbps){
  if(!finite(mbps)||mbps<=0)return .2;
  return clamp(Math.log10(1+mbps)/Math.log10(1+180),.08,1);
}
/** Particle count from latency (ms): a short round trip is a dense stream. */
export function densityFor(latencyMs){
  const base=finite(latencyMs)?1-clamp(latencyMs,0,250)/250*.7:.6;
  return Math.round(MAX_PARTICLES*clamp(base,.3,1));
}
const route=path=>path==='relay'?'relay':path==='direct'||path==='lan'?'lan':'unknown';
const elapsedText=s=>!finite(s)?'':s<60?`${Math.floor(s)} s`:s<3600?`${Math.floor(s/60)} min ${Math.floor(s%60)} s`:`${Math.floor(s/3600)} h ${Math.floor(s%3600/60)} min`;

/** The one job the animation follows: the most recent moving one (or one that just landed). */
export function activeJob(activity){
  const jobs=Array.isArray(activity?.jobs)?activity.jobs.filter(j=>j&&typeof j.phase==='string'):[];
  return jobs.find(j=>j.phase!=='landed'&&j.phase!=='ended')||jobs.find(j=>j.phase==='landed')||null;
}

/**
 * Visual parameters for one moment.
 * @param {object} input
 * @param {'online'|'paused'|'offline'} input.health  the Linked card's health (linkHealth)
 * @param {string} [input.path]                       Spark Link path: direct | lan | relay
 * @param {object} [input.activity]                   GET /api/compute/spark-activity
 * @param {boolean} [input.reduced]                   prefers-reduced-motion
 * @param {number} [input.reconnectedAgoS]            seconds since the link came back (null: no recent outage)
 * @param {string} [input.previousRoute]              the route before the last path change
 */
export function linkMotion({health='offline',path=null,activity=null,reduced=false,reconnectedAgoS=null,previousRoute=null}={}){
  const r=route(activity?.link?.path==='relay'?'relay':activity?.link?.path==='lan'?'lan':path);
  const link=activity?.link||{};
  const {throughput,measured}=linkSpeed(link);
  const job=activeJob(activity);
  const lost=health==='offline'||job?.phase==='unreachable'||activity?.reachable===false&&activity?.linked===true;
  const state=health==='paused'?'paused':lost?'disconnected':finite(reconnectedAgoS)&&reconnectedAgoS<REKNIT_S?'reconnected':job?'job':'idle';
  const filament={
    intact:state!=='disconnected',
    frozen:state==='paused',
    // Knit fraction 0..1 while re-knitting after a reconnect.
    knit:state==='reconnected'?clamp(reconnectedAgoS/REKNIT_S,0,1):state==='disconnected'?0:1,
    speed:state==='paused'||state==='disconnected'?0:speedFor(throughput),
    particles:state==='paused'?Math.round(densityFor(link.latency_ms)*.5):densityFor(link.latency_ms),
    dashed:r==='relay',
    saturation:state==='paused'?0:state==='disconnected'?.25:1,
    slow:link.slow===true||(finite(throughput)&&throughput<SLOW_MBPS),
    measured,
  };
  const relayNode={visible:r==='relay',pulse:r==='relay'&&state!=='paused'&&state!=='disconnected'};
  const utilization=finite(activity?.spark_gpu_utilization)?clamp(activity.spark_gpu_utilization/100,0,1):null;
  const spark={
    // Breathing only from reported GPU utilization (the node does not report it yet: null, no breath).
    breathe:state==='idle'&&utilization!==null?utilization:null,
    glow:job&&state==='job'&&job.phase==='running'?(finite(job.progress)?clamp(job.progress,0,1):.5):job&&state==='job'&&['downloading','importing','landed'].includes(job.phase)?1:0,
    indeterminate:!!(job&&job.phase==='running'&&!finite(job.progress)),
  };
  let packet=null,output=null;
  if(job&&(state==='job'||state==='reconnected'||state==='disconnected')){
    if(job.phase==='uploading')packet={direction:'up',fill:clamp(finite(job.upload)?job.upload:0,0,1)};
    if(['downloading','importing'].includes(job.phase))packet={direction:'down',fill:clamp(finite(job.download)?job.download:0,0,1)};
    if(job.phase==='unreachable')packet={direction:finite(job.download)&&job.download>0?'down':finite(job.upload)&&job.upload<1?'up':'hold',fill:clamp(finite(job.download)&&job.download>0?job.download:finite(job.upload)?job.upload:0,0,1),stalled:true};
    if(['downloading','importing','landed'].includes(job.phase))output={fill:job.phase==='downloading'?clamp(finite(job.download)?job.download:0,0,1):1,settled:job.phase==='landed'};
  }
  const morph=activity?.path_change&&activity.path_change.from!==activity.path_change.to?{from:activity.path_change.from,to:activity.path_change.to}
    :previousRoute&&previousRoute!==r&&r!=='unknown'?{from:previousRoute,to:r}:null;
  const message=state==='disconnected'?(job&&finite(job.unreachable_s)?`Spark unreachable, waiting to reconnect (${elapsedText(job.unreachable_s)})`:'Spark unreachable, waiting to reconnect')
    :state==='paused'?'Remote access paused'
    :state==='reconnected'?(job?'Reconnected; following the same job':'Reconnected')
    :job?jobLine(job)
    :finite(throughput)?`${r==='relay'?'Relay':'LAN'} · ~${throughput>=10?Math.round(throughput):throughput.toFixed(1)} MB/s${measured?'':' (estimated)'}${finite(link.latency_ms)?` · ${Math.round(link.latency_ms)} ms`:''}`
    :null;
  // Motion runs only when something moves: never with reduced motion or while paused. A snapped
  // filament drifts once; the renderer then stops and releases its canvas.
  const animate=!reduced&&state!=='paused';
  return {state,route:r,filament,relayNode,spark,packet,output,morph,message,animate,reduced:!!reduced};
}

/**
 * The link speed shown (and driving the particles) and whether it was measured. An unmeasured
 * direction carries its path's prior, so a speed is "measured" only when a measured direction gives
 * it: the backend's own figure (`throughput_mbps`: the measured direction wins) with its
 * per-direction flags. Without them (an older backend) the one `measured` flag vouches for the
 * shown speed only when both directions read the same; otherwise it is labelled estimated.
 */
export function linkSpeed(link={}){
  const up=link.up_mbps,down=link.down_mbps;
  const lower=finite(up)&&finite(down)?Math.min(up,down):finite(up)?up:finite(down)?down:null;
  if(typeof link.up_measured==='boolean'||typeof link.down_measured==='boolean'){
    const upM=link.up_measured===true,downM=link.down_measured===true;
    const own=upM&&!downM&&finite(up)?up:downM&&!upM&&finite(down)?down:lower;
    return {throughput:finite(link.throughput_mbps)?link.throughput_mbps:own,measured:upM||downM};
  }
  return {throughput:lower,measured:link.measured===true&&(!finite(up)||!finite(down)||up===down)};
}

function jobLine(job){
  const pct=v=>`${Math.round(clamp(v,0,1)*100)}%`;
  switch(job.phase){
    case 'uploading':return `Sending to the Spark · ${pct(job.upload??0)}`;
    case 'running':return finite(job.progress)?`Running on the Spark · ${pct(job.progress)}`:'Running on the Spark';
    case 'downloading':return `Bringing results back · ${pct(job.download??0)}`;
    case 'importing':return 'Checking and importing results';
    case 'landed':return 'Results imported';
    default:return null;
  }
}

import test from 'node:test';
import assert from 'node:assert/strict';
import {connectionView,configMode,jobsFrom,jobPage,resourcePolicy,agentPrompt,agentConfiguration} from '../ui/model.mjs';
import {linkMotion} from '../ui/vendor/sparkLinkMotion.mjs';
import {filamentParticles,filamentFrame} from '../ui/vendor/sparkLinkParticles.mjs';
import {startVisibleRenderLoop} from '../ui/vendor/visible-render-loop.mjs';

const lan={ok:true,devices:[{id:'spark',reachable:true,transport:{route:'lan_ssh',authenticated:true},scheduler:{healthy:true}}]};
test('connection is never claimed from reachability without authentication',()=>{
  const forged=structuredClone(lan);forged.devices[0].transport.authenticated=false;
  assert.equal(connectionView(forged,{}).connected,false);
  assert.equal(connectionView(lan,{}).route,'Pinned SSH · LAN');
});
test('disconnection wins over a stale successful observation and legacy mode migrates',()=>{
  const state=connectionView(lan,{connection_enabled:false,allow_remote:true});
  assert.equal(state.connected,false);assert.equal(state.state,'disconnected');assert.equal(state.mode,'both');
  assert.equal(configMode({connection_mode:'remote-only',allow_remote:false}),'remote-only');
});
test('paired routing does not invent relay or hotspot proof',()=>{
  const remote=structuredClone(lan);remote.devices[0].transport.route='paired_iroh';
  assert.equal(connectionView(remote,{connection_mode:'both'}).route,'Paired connection · path unknown');
});
test('changing mode invalidates a previously observed forbidden route during refresh',()=>{
  const state=connectionView(lan,{connection_mode:'remote-only'});
  assert.equal(state.connected,false);assert.equal(state.state,'error');assert.match(state.reason,/fresh connection check/);assert.notEqual(state.route,'Pinned SSH · LAN');
  assert.equal(connectionView(null,{connection_mode:'remote-only'},true).state,'connecting');
  const remote=structuredClone(lan);remote.devices[0].transport.route='paired_iroh';
  assert.equal(connectionView(remote,{connection_mode:'lan-only'}).connected,false);
});
test('board deduplicates shared laptop records and pagination clamps after deletion',()=>{
  const jobs=Array.from({length:19},(_,n)=>({job:`j${n}`,created_at:n}));
  const combined=jobsFrom({shared_board:jobs,devices:[{id:'laptop',jobs:[jobs[0]]}]});
  assert.equal(combined.length,19);assert.equal(jobPage(combined,3).rows.length,1);
  const reduced=jobPage(combined.slice(0,4),3);assert.equal(reduced.page,0);assert.equal(reduced.rows.length,4);
});
const valid={cpu_per_job:4,memory_gb_per_job:8,cpu_total:16,memory_gb_total:64,client_jobs:2,client_cpu:8,headroom_gb:32};
const form=value=>new Map(Object.entries(value).map(([k,v])=>[k,String(v)]));
test('resource editor rejects impossible budgets and missing fields before calling backend',()=>{
  assert.deepEqual(resourcePolicy(form(valid)),valid);
  assert.throws(()=>resourcePolicy(form({...valid,client_cpu:2})),/CPU per job/);
  assert.throws(()=>resourcePolicy(form({...valid,memory_gb_per_job:65})),/Memory per job/);
  assert.throws(()=>resourcePolicy(form({...valid,headroom_gb:0})),/positive whole/);
  assert.throws(()=>resourcePolicy(form({...valid,cpu_total:2.5})),/whole/);
  assert.throws(()=>resourcePolicy(new Map()),/whole/);
});
test('copied prompt selects safe references and keeps caller and state identity explicit',()=>{
  const config={ssh_alias:'my-spark',hosts:['192.0.2.10'],identity_file:'C:/keys/spark',user:'owner',port:22,connection_mode:'both',private_key:'DO_NOT_COPY_KEY',verification_code:'DO_NOT_COPY_CODE',ticket:{token:'DO_NOT_COPY_TOKEN'},password:'DO_NOT_COPY_PASSWORD'};
  const text=agentPrompt({agent:'claude',path:'C:/Program Files/DGXBridge/bin/sparkdev.exe',stateRoot:'\\\\localhost\\C$\\Users\\Example\\AppData\\Local\\SparkDevBridge',config,connection:connectionView(lan,config),limits:valid});
  assert.match(text,/client label claude/);assert.match(text,/--client claude --json run/);assert.match(text,/SPARKDEV_HOME/);assert.match(text,/LocalCache/);assert.match(text,/Private-key file reference: C:\/keys\/spark/);assert.doesNotMatch(text,/DO_NOT_COPY/);assert.match(text,/--request-id RETAINED_ID/);assert.match(text,/next_offset/);assert.match(text,/no enforced CPU or memory cap/);
});
test('MCP snippets preserve quoted paths and explicit physical owner-state override',()=>{
  const path='C:\\Tools with spaces\\sparkdev.exe',root='\\\\localhost\\C$\\Users\\Example\\AppData\\Local\\SparkDevBridge';
  const claude=JSON.parse(agentConfiguration('claude',path,root));assert.equal(claude.mcpServers.sparkdev.env.SPARKDEV_HOME,root);assert.deepEqual(claude.mcpServers.sparkdev.args,['--client','claude','mcp']);
  const codex=agentConfiguration('codex',path,root);assert.match(codex,/\[mcp_servers.sparkdev.env\]/);assert.match(codex,/SPARKDEV_HOME = /);
});
test('real motion defaults contain no measured traffic, job progress or GPU utilization',()=>{
  const motion=linkMotion({health:'online',path:'lan'});assert.equal(motion.filament.measured,false);assert.equal(motion.packet,null);assert.equal(motion.spark.breathe,null);assert.equal(motion.message,null);assert.equal(motion.animate,true);
  const reduced=linkMotion({health:'online',path:'lan',reduced:true});assert.equal(reduced.animate,false);
  assert.equal(linkMotion({health:'paused'}).animate,false);
  const dots=filamentFrame(filamentParticles(40),motion,{width:500,height:100,dt:.016});assert.equal(dots.length,40);assert.ok(dots.every(dot=>Number.isFinite(dot.x)&&Number.isFinite(dot.y)));
});
test('visibility lifecycle cancels hidden/offscreen animation and disposes observers',()=>{
  let callback,observer,frameId=0,cancelled=0,drawn=0,disconnected=0;const host={};
  const document={hidden:false,addEventListener(_,cb){callback=cb;},removeEventListener(){callback=null;}};
  class Observer{constructor(cb){observer=cb;}observe(){}disconnect(){disconnected++;}}
  const frames=new Map();const loop=startVisibleRenderLoop(host,()=>drawn++,{document,IntersectionObserver:Observer,requestFrame:cb=>{frames.set(++frameId,cb);return frameId;},cancelFrame:id=>{frames.delete(id);cancelled++;}});
  assert.equal(frames.size,0);observer([{target:host,isIntersecting:true}]);assert.equal(frames.size,1);
  document.hidden=true;callback();assert.equal(frames.size,0);assert.equal(cancelled,1);assert.equal(drawn,0);
  document.hidden=false;callback();assert.equal(frames.size,1);loop.dispose();assert.equal(frames.size,0);assert.equal(disconnected,1);assert.equal(callback,null);
});

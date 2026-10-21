import test from 'node:test';
import assert from 'node:assert/strict';
import {connectionView,configMode,jobsFrom,jobPage,resourcePolicy,resourcePreset,RESOURCE_PRESETS,sameResourcePolicy,agentPaths,agentPrompt,agentConfiguration,routeAddress,routeAddressLabel,
  FIND_INTRO,LAN_UNREACHABLE,findSparkControl,discoveryView,discoverySaved,otherLanAddresses} from '../ui/model.mjs';
import {linkMotion} from '../ui/vendor/sparkLinkMotion.mjs';
import {filamentParticles,filamentFrame} from '../ui/vendor/sparkLinkParticles.mjs';
import {startVisibleRenderLoop} from '../ui/vendor/visible-render-loop.mjs';

const lan={ok:true,devices:[{id:'spark',reachable:true,transport:{route:'lan_ssh',authenticated:true},scheduler:{healthy:true}}]};
test('connection is never claimed from reachability without authentication',()=>{
  const forged=structuredClone(lan);forged.devices[0].transport.authenticated=false;
  assert.equal(connectionView(forged,{}).connected,false);
  assert.equal(connectionView(lan,{}).route,'Pinned SSH · LAN');
});
test('address used names the dialed host and its configured position whatever its source',()=>{
  const config={ssh_alias:'my-spark',hosts:['192.0.2.186','192.0.2.187']};
  const seen=(attempt,lanHost=attempt.host)=>({ok:true,devices:[{id:'spark',transport:{route:'lan_ssh',authenticated:true,...(lanHost?{lan_host:lanHost}:{}),attempted_routes:[{route:'lan_ssh',status:'host_unreachable',source:'configured',host:'192.0.2.186',target_index:0},{route:'lan_ssh',status:'authenticated',...attempt}]}}]});
  // A remembered configured address carries no target_index of its own.
  const remembered=routeAddress(seen({source:'remembered',host:'192.0.2.187',target_index:null}),config);
  assert.deepEqual(remembered,{address:'192.0.2.187',index:1,of:2});
  assert.equal(routeAddressLabel(remembered),'192.0.2.187 (configured address 2 of 2)');
  assert.deepEqual(routeAddress(seen({source:'sticky',host:'spark-b.local',target_index:null}),{...config,hosts:['spark-a.local','SPARK-B.local']}),{address:'spark-b.local',index:1,of:2});
  // An address outside the configured list gets no position suffix.
  const alias=routeAddress(seen({source:'alias',host:'192.0.2.9',target_index:null}),config);
  assert.deepEqual(alias,{address:'192.0.2.9',index:null,of:2});
  assert.equal(routeAddressLabel(alias),'192.0.2.9');
  // An alias OpenSSH resolved reports no host: name the alias, no suffix.
  const bare=routeAddress(seen({source:'alias',host:null,target_index:null},null),config);
  assert.deepEqual(bare,{address:'my-spark',index:null,of:2});
  assert.equal(routeAddressLabel(bare),'my-spark');
  // Older observations without a dialed host still use target_index.
  const legacy={ok:true,devices:[{id:'spark',transport:{route:'lan_ssh',authenticated:true,attempted_routes:[{route:'lan_ssh',status:'authenticated',target_index:1}]}}]};
  assert.equal(routeAddressLabel(routeAddress(legacy,config)),'192.0.2.187 (configured address 2 of 2)');
  assert.equal(routeAddressLabel(routeAddress(legacy,{...config,hosts:['192.0.2.186']})),'my-spark');
  assert.equal(routeAddressLabel(null),'—');
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
  assert.throws(()=>resourcePolicy(form(valid),{cpu_count:12,memory_gb:119}),/12 CPU cores/);
  assert.throws(()=>resourcePolicy(form({...valid,headroom_gb:60}),{cpu_count:20,memory_gb:119}),/plus headroom/);
});
test('Builds first has the reviewed Spark budget and presets scale down within observed hardware',()=>{
  assert.deepEqual(resourcePreset('builds-first',{cpu_count:20,memory_gb:119}),{cpu_per_job:16,memory_gb_per_job:32,cpu_total:18,memory_gb_total:80,client_jobs:4,client_cpu:16,headroom_gb:32});
  for(const hardware of [{cpu_count:20,memory_gb:119},{cpu_count:1,memory_gb:2},{cpu_count:4,memory_gb:16},{cpu_count:8,memory_gb:32},{cpu_count:128,memory_gb:256}]){
    for(const preset of RESOURCE_PRESETS){const policy=resourcePreset(preset.id,hardware);assert.deepEqual(resourcePolicy(form(policy),hardware),policy);assert.ok(policy.cpu_total<=hardware.cpu_count);assert.ok(policy.memory_gb_total+policy.headroom_gb<=hardware.memory_gb);}
  }
  assert.throws(()=>resourcePreset('balanced',null),/Read the Spark hardware/);
  assert.throws(()=>resourcePreset('invented',{cpu_count:20,memory_gb:119}),/known resource preset/);
});
test('resource draft equality uses policy fields rather than response metadata',()=>{
  assert.equal(sameResourcePolicy({...valid,other:'ignored'},Object.fromEntries(form(valid))),true);
  assert.equal(sameResourcePolicy(valid,{...valid,cpu_per_job:3}),false);
  assert.equal(sameResourcePolicy(valid,null),false);
});
test('agent path selection keeps Claude on verified ordinary paths and Codex on verified UNC paths',()=>{
  const plain={cli_path:'C:/Owner/DGXBridge/bin/sparkdev.exe',state_root:'C:/Owner/SparkDevBridge'},unc={cli_path:String.raw`\\localhost\C$\Owner\DGXBridge\bin\sparkdev.exe`,state_root:String.raw`\\localhost\C$\Owner\SparkDevBridge`};
  const preferences={...plain,agent_cli_path:unc.cli_path,agent_state_root:unc.state_root,agent_paths:{claude:plain,codex:unc}};
  assert.deepEqual(agentPaths('claude',preferences),{path:plain.cli_path,stateRoot:plain.state_root});
  assert.deepEqual(agentPaths('codex',preferences),{path:unc.cli_path,stateRoot:unc.state_root});
  delete preferences.agent_paths;
  assert.deepEqual(agentPaths('claude',preferences),{path:plain.cli_path,stateRoot:plain.state_root});
  assert.deepEqual(agentPaths('codex',preferences),{path:unc.cli_path,stateRoot:unc.state_root});
  assert.equal(agentPaths('claude',{cli_path:'unsafe\nreference'}).path,'sparkdev');
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
test('copied MCP commands and prompts preserve native UNC and verbatim path syntax',()=>{
  const root=String.raw`\\localhost\C$\Users\Example\AppData\Local\SparkDevBridge`;
  for(const path of [String.raw`C:\Tools with spaces\sparkdev.exe`,String.raw`\\localhost\C$\Tools with spaces\sparkdev.exe`,String.raw`\\?\C:\Tools with spaces\sparkdev.exe`,String.raw`\\?\UNC\server\share\sparkdev.exe`]) {
    const claude=JSON.parse(agentConfiguration('claude',path,root));
    assert.equal(claude.mcpServers.sparkdev.command,path);
    assert.equal(claude.mcpServers.sparkdev.env.SPARKDEV_HOME,root);
    const codex=agentConfiguration('codex',path,root);
    const commandLine=codex.split('\n').find(line=>line.startsWith('command = '));
    assert.equal(JSON.parse(commandLine.slice('command = '.length)),path);
    const prompt=agentPrompt({agent:'codex',path,stateRoot:root});
    assert.ok(prompt.includes(`Installed CLI: ${path}\n`));
    assert.doesNotMatch(prompt,/\/\/\?\//);
  }
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
test('Find my Spark is offered only when an owner search can run, and suggested when the LAN is unreachable',()=>{
  const config={ssh_alias:'fixture-spark',hosts:['192.0.2.20'],connection_mode:'both',connection_enabled:true};
  const ready=findSparkControl({config,connection:connectionView(lan,config)});
  assert.deepEqual(ready,{disabled:false,reason:'',suggested:false});
  for(const [state,pattern] of [[{config:null},/Save your SSH connection/],[{config:{...config,connection_enabled:false}},/Connect first/],[{config:{...config,connection_mode:'remote-only'}},/LAN only or Both/]]){
    const control=findSparkControl(state);assert.equal(control.disabled,true);assert.match(control.reason,pattern);assert.equal(control.suggested,false);
  }
  assert.equal(findSparkControl({config,connecting:true}).disabled,true);
  assert.equal(findSparkControl({config,busy:true}).disabled,true);
  const unreachable={ok:true,devices:[{id:'spark',reachable:false,error:{message:LAN_UNREACHABLE},transport:{route:'none',authenticated:false}}]};
  assert.equal(findSparkControl({config,connection:connectionView(unreachable,config)}).suggested,true);
  assert.equal(findSparkControl({config:{...config,connection_mode:'remote-only'},connection:connectionView(unreachable,config)}).suggested,false);
  assert.match(FIND_INTRO,/Nothing is saved until you choose an address/);
});
test('Find my Spark results show only sanitized found addresses, plain notes and honest empty copy',()=>{
  const config={hosts:['192.0.2.20','192.0.2.41']};
  const result={ok:true,found:[{host:'192.0.2.41',adapter:'Ethernet',medium:'wired',configured_index:1},{host:'192.0.2.40',adapter:'Wi-Fi\n<img src=x>',medium:'wifi',configured_index:null},{host:'<script>',adapter:'x',medium:'wired'},{host:'192.0.2.40',adapter:'dup',medium:'wired'},{host:'192.0.2.300',adapter:'bad',medium:'wired'}],
    configured:[{index:0,host:'192.0.2.20',status:'other_host'},{index:1,host:'192.0.2.41',status:'verified'}],login_rejected:2,local_key_error:1,other_ssh_hosts:3,unknown_key_hosts:1,unchecked:4,not_checked:1,pin_hint:false};
  const view=discoveryView(result,config);
  assert.deepEqual(view.rows.map(row=>[row.host,row.adapterLabel,row.mediumLabel,row.configuredIndex,row.alreadyFirst]),[['192.0.2.41','Ethernet','wired',1,false],['192.0.2.40','network adapter','Wi-Fi',null,false]]);
  assert.match(view.rows[0].detail,/this laptop's Ethernet \(wired\) · saved as LAN address 2/);
  assert.match(view.summary,/2 addresses/);assert.match(view.summary,/cannot yet tell which one is the Spark's wired connection/);
  assert.deepEqual(view.conflicts,['192.0.2.20']);assert.equal(view.loginRejected,2);assert.equal(view.localKeyError,1);assert.equal(view.pinHint,false);assert.equal(view.emptyReason,'');
  const notes=view.notes.join('\n');
  assert.match(notes,/192\.0\.2\.20 now answers as a different SSH device/);assert.match(notes,/2 addresses refused the SSH login/);assert.match(notes,/file permissions/);assert.match(notes,/3 other SSH devices answered and were left alone/);assert.match(notes,/4 addresses could not be checked in time/);assert.match(notes,/1 more address answered but was not checked/);
  assert.doesNotMatch(JSON.stringify(view),/<script>|<img/);
  // The current saved list, not the search-time index, decides "already first".
  const first=discoveryView({found:[{host:'192.0.2.41',adapter:'Ethernet',medium:'wired',configured_index:1}]},{hosts:['192.0.2.41']});
  assert.equal(first.rows[0].alreadyFirst,true);assert.match(first.summary,/^Found your Spark at 192\.0\.2\.41\.$/);assert.match(first.rows[0].detail,/already your first LAN address/);
  assert.equal(discoveryView({found:[{host:'192.0.2.41',configured_index:0}]}).rows[0].alreadyFirst,true);
  const pin=discoveryView({found:[],pin_hint:true,configured:[{index:0,host:'192.0.2.20',status:'unknown_key'}]},config);
  assert.equal(pin.pinHint,true);assert.match(pin.notes.join(' '),/known_hosts entry yourself/);assert.match(pin.notes.join(' '),/never changes it/);assert.deepEqual(pin.conflicts,[]);
  const empty=discoveryView({ok:true,found:[]},config);
  assert.match(empty.summary,/did not find your Spark/);
  for(const cause of [/guest network or client isolation/,/VPN/,/\/24/,/Hyper-V virtual switch/])assert.match(empty.emptyReason,cause);
  assert.match(discoveryView({found:[],reason:'no_physical_adapter'}).emptyReason,/no wired or Wi-Fi connection/);
  assert.match(discoveryView({found:[],reason:'no_routed_targets'}).emptyReason,/Pause the VPN/);
  assert.deepEqual(discoveryView(null).rows,[]);
});
test('a saved address names what was removed and the Other LAN addresses field keeps every address',()=>{
  assert.equal(discoverySaved({host:'192.0.2.40',removed:[]},'192.0.2.40'),'Saved 192.0.2.40 as the first LAN address DGXBridge tries.');
  const text=discoverySaved({host:'192.0.2.40',removed:['192.0.2.20','192.0.2.21\nx'],warning:'You now have 5 saved LAN addresses, tried in order.'},'192.0.2.40');
  assert.match(text,/Removed 192\.0\.2\.20, which now answers as a different SSH device\./);assert.match(text,/5 saved LAN addresses/);assert.doesNotMatch(text,/\n/);
  assert.deepEqual(otherLanAddresses('192.0.2.21, 192.0.2.22 192.0.2.21;spark-b.local,,192.0.2.20','192.0.2.20'),['192.0.2.21','192.0.2.22','spark-b.local']);
  assert.deepEqual(otherLanAddresses('',''),[]);assert.deepEqual(otherLanAddresses(null),[]);
});
test('copied agent prompts forbid Find my Spark',()=>{
  for(const agent of ['claude','codex'])assert.match(agentPrompt({agent,config:{hosts:['192.0.2.10']}}),/Never run sparkdev discover; Find my Spark is an owner-only action\./);
});

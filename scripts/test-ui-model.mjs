import test from 'node:test';
import assert from 'node:assert/strict';
import {connectionView,configMode,jobsFrom,reconcileObservedJobs,jobPage,resourcePolicy,resourcePreset,RESOURCE_PRESETS,sameResourcePolicy,agentPaths,agentPrompt,agentConfiguration,routeAddress,routeAddressLabel,plainReason,internetView,downloadsView,serviceView,jobFlags,jobInternet,laptopLink,sparkLink,
  FIND_INTRO,LAN_UNREACHABLE,findSparkControl,discoveryView,discoverySaved,otherLanAddresses,isActiveJob,jobOutcome,jobRunner,jobRunnerLabel,jobToolsLabel,filterJobs,boardSummary,namedComputers,deviceRows,fleetSummary,deviceSlug,deviceSetupCommand,probeSummary} from '../ui/model.mjs';
import {linkMotion} from '../ui/vendor/sparkLinkMotion.mjs';
import {filamentParticles,filamentFrame} from '../ui/vendor/sparkLinkParticles.mjs';
import {startVisibleRenderLoop} from '../ui/vendor/visible-render-loop.mjs';

const lan={ok:true,devices:[{id:'spark',reachable:true,transport:{route:'lan_ssh',authenticated:true},scheduler:{healthy:true}}]};
test('connection is never claimed from reachability without authentication',()=>{
  const forged=structuredClone(lan);forged.devices[0].transport.authenticated=false;
  assert.equal(connectionView(forged,{}).connected,false);
  assert.equal(connectionView(lan,{}).route,'Same network');
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
  assert.equal(connectionView(remote,{connection_mode:'both'}).route,'Remote link');
});
test('changing mode invalidates a previously observed forbidden route during refresh',()=>{
  const state=connectionView(lan,{connection_mode:'remote-only'});
  assert.equal(state.connected,false);assert.equal(state.state,'error');assert.match(state.reason,/Checking again/);assert.notEqual(state.route,'Same network');
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
  assert.throws(()=>resourcePolicy(form({...valid,headroom_gb:60}),{cpu_count:20,memory_gb:119}),/plus memory kept free/);
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
  for(const [state,pattern] of [[{config:null},/Save your SSH connection/],[{config:{...config,connection_enabled:false}},/Connect first/],[{config:{...config,connection_mode:'remote-only'}},/Same network only or Automatic/]]){
    const control=findSparkControl(state);assert.equal(control.disabled,true);assert.match(control.reason,pattern);assert.equal(control.suggested,false);
  }
  assert.equal(findSparkControl({config,connecting:true}).disabled,true);
  assert.equal(findSparkControl({config,busy:true}).disabled,true);
  const lanTry=(status,host='192.0.2.20')=>({route:'lan_ssh',attempted:true,status,reason:'fixture',target_index:0,source:'configured',host});
  const spark=(fields,attempts)=>({ok:true,devices:[{id:'spark',reachable:false,...fields,transport:{selected:'unselected',route:'none',authenticated:false,reason:'No authenticated request yet',attempted_routes:attempts,...fields.transport}}]});
  const lanOnly={...config,connection_mode:'lan-only'};
  const lanOnlyDown=spark({error:{code:4,message:LAN_UNREACHABLE,kind:'transport_unavailable'}},[lanTry('host_unreachable'),{route:'paired_iroh',attempted:false,status:'disabled',reason:'Remote transport is disabled in local configuration',target_index:null}]);
  assert.equal(findSparkControl({config:lanOnly,connection:connectionView(lanOnlyDown,lanOnly)}).suggested,true);
  for(const [name,observation,suggested] of [
    ['no sidecar',spark({error:{code:3,message:'sparkdev-link sidecar is not installed beside sparkdev',kind:'remote_adapter_unconfigured'}},[lanTry('host_unreachable'),lanTry('host_unreachable','192.0.2.21'),{route:'paired_iroh',attempted:false,status:'unconfigured',reason:'Remote adapter is not installed',target_index:null}]),true],
    ['paired failure',spark({error:{code:4,message:'Authenticated remote sidecar unavailable, busy or not paired; retry the same request ID',kind:'transport_unavailable'}},[lanTry('host_unreachable'),{route:'paired_iroh',attempted:true,status:'remote_unavailable',reason:'Remote adapter reported failure; physical network path is unknown',target_index:null}]),true],
    ['paired success',spark({reachable:true,scheduler:{healthy:true},transport:{selected:'iroh',route:'paired_iroh',authenticated:true,reason:'LAN unreachable; paired endpoint transport explicitly enabled'}},[lanTry('host_unreachable'),{route:'paired_iroh',attempted:true,status:'authenticated',reason:'Paired endpoint authenticated',target_index:null}]),true],
    ['LAN answered',lan,false],
    ['LAN dropped',spark({error:{code:3,message:'sparkdev-link sidecar is not installed beside sparkdev'}},[{...lanTry('connection_dropped'),phase:'handshake',try:1,retry:'exhausted'},{route:'paired_iroh',attempted:false,status:'unconfigured',target_index:null}]),false],
    ['LAN rejected',spark({error:{code:7,message:'SSH authentication or pinned host-key verification failed; fallback refused'}},[lanTry('authentication_failed')]),false],
  ]){
    assert.equal(findSparkControl({config,connection:connectionView(observation,config)}).suggested,suggested,name);
    assert.doesNotMatch(JSON.stringify(observation),new RegExp(LAN_UNREACHABLE),name);
  }
  assert.equal(findSparkControl({config:{...config,connection_mode:'remote-only'},connection:connectionView(lanOnlyDown,config)}).suggested,false);
  assert.match(FIND_INTRO,/Nothing is saved until you choose an address/);
});
test('Find my Spark results show only sanitized found addresses, plain notes and honest empty copy',()=>{
  const config={hosts:['192.0.2.20','192.0.2.41']};
  const result={ok:true,found:[{host:'192.0.2.41',adapter:'Ethernet',medium:'wired',configured_index:1},{host:'192.0.2.40',adapter:'Wi-Fi\n<img src=x>',medium:'wifi',configured_index:null},{host:'<script>',adapter:'x',medium:'wired'},{host:'192.0.2.40',adapter:'dup',medium:'wired'},{host:'192.0.2.300',adapter:'bad',medium:'wired'}],
    configured:[{index:0,host:'192.0.2.20',status:'other_host'},{index:1,host:'192.0.2.41',status:'verified'}],login_rejected:2,local_key_error:1,other_ssh_hosts:3,unknown_key_hosts:1,unchecked:4,not_checked:1,pin_hint:false};
  const view=discoveryView(result,config);
  assert.deepEqual(view.rows.map(row=>[row.host,row.adapterLabel,row.mediumLabel,row.configuredIndex,row.alreadyFirst]),[['192.0.2.41','Ethernet','wired',1,false],['192.0.2.40','network adapter','Wi-Fi',null,false]]);
  assert.match(view.rows[0].detail,/this laptop's Ethernet \(wired\) · saved as LAN address 2/);
  assert.match(view.summary,/2 addresses/);assert.match(view.summary,/connection type is not reported/);
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
  assert.match(discoveryView({found:[],reason:'no_physical_adapter',unverified:1}).emptyReason,/no wired or Wi-Fi connection/);
  assert.match(discoveryView({found:[],reason:'no_routed_targets'}).emptyReason,/Pause the VPN/);
  assert.deepEqual(discoveryView(null).rows,[]);
});
test('an empty search says when addresses answered but could not be confirmed',()=>{
  const config={hosts:['192.0.2.20']};
  const base={ok:true,saved:false,open:1,unchecked:0,found:[],login_rejected:0,local_key_error:0,configured:[{index:0,host:'192.0.2.20',status:'closed'}],other_ssh_hosts:0,unknown_key_hosts:0,unverified:0,not_checked:0,pin_hint:false};
  const empty=discoveryView({...base,open:0},config);
  assert.match(empty.emptyReason,/did not answer/);assert.deepEqual(empty.notes,[]);
  // The configured address answered on the SSH port, but its pinned check reset or timed out.
  const broke=discoveryView({...base,unverified:1,configured:[{index:0,host:'192.0.2.20',status:'unverified'}]},config);
  assert.match(broke.summary,/did not find your Spark/);assert.equal(broke.unverified,1);
  assert.doesNotMatch(broke.emptyReason,/did not answer|client isolation|VPN/);assert.match(broke.emptyReason,/answered on the SSH port/);
  assert.match(broke.notes.join(' '),/1 address answered on the SSH port, but the pinned check broke off or did not finish in time\. Search again\./);
  // An address the owner's SSH settings route elsewhere was never checked.
  const routed=discoveryView({...base,not_direct:2,configured:[{index:0,host:'192.0.2.20',status:'not_direct'}]},config);
  assert.equal(routed.notDirect,2);assert.match(routed.emptyReason,/none could be confirmed/);
  assert.match(routed.notes.join(' '),/2 answering addresses would be reached through a proxy, jump host or bind option in your SSH settings, so Find my Spark did not check them\./);
  // A refused login no longer sits under copy that says nothing answered.
  const refused=discoveryView({...base,login_rejected:1},config);
  assert.match(refused.emptyReason,/none could be confirmed/);assert.match(refused.notes.join(' '),/1 address refused the SSH login/);
  assert.match(discoveryView({...base,local_key_error:2},config).emptyReason,/none could be confirmed/);
  // An unknown key at an address that is not saved, with no pin hint, is still shown.
  const unknown=discoveryView({...base,unknown_key_hosts:2},config);
  assert.equal(unknown.unknownKey,2);assert.match(unknown.emptyReason,/none could be confirmed/);
  assert.match(unknown.notes.join(' '),/2 SSH devices answered with a host key that does not match the one this laptop has saved for your Spark\. If you reinstalled the Spark, update its known_hosts entry yourself/);
  // With the pin hint the unknown key is said once, not twice.
  const hinted=discoveryView({...base,unknown_key_hosts:1,pin_hint:true,configured:[{index:0,host:'192.0.2.20',status:'unknown_key'}]},config);
  assert.equal(hinted.notes.filter(note=>/known_hosts/.test(note)).length,1);
  // Beside a found Spark, other unknown keys are only other devices.
  const beside=discoveryView({...base,found:[{host:'192.0.2.40',adapter:'Ethernet',medium:'wired',configured_index:null}],unknown_key_hosts:1},config);
  assert.match(beside.notes.join(' '),/1 SSH device answered with a host key that does not match the one this laptop has saved for your Spark and was left alone\./);assert.doesNotMatch(beside.notes.join(' '),/reinstalled/);
});
test('a conflicting saved address names a removal the page can actually do',()=>{
  const config={hosts:['192.0.2.41','192.0.2.20']};
  const conflict=[{index:0,host:'192.0.2.41',status:'verified'},{index:1,host:'192.0.2.20',status:'other_host'}];
  // The only verified address is already first: it has no Use this address button.
  const first=discoveryView({found:[{host:'192.0.2.41',adapter:'Ethernet',medium:'wired',configured_index:0}],configured:conflict,other_ssh_hosts:1},config);
  assert.equal(first.rows.length,1);assert.equal(first.rows[0].alreadyFirst,true);
  const firstNotes=first.notes.join(' ');
  assert.match(firstNotes,/192\.0\.2\.20 now answers as a different SSH device\. Remove it under Saved SSH connection, Other LAN addresses\./);assert.doesNotMatch(firstNotes,/address below/);
  // A refused login proved the pin, but nothing was verified: no rows at all.
  const none=discoveryView({found:[],configured:[{index:0,host:'192.0.2.41',status:'login_rejected'},{index:1,host:'192.0.2.20',status:'other_host'}],login_rejected:1,other_ssh_hosts:1},config);
  assert.equal(none.rows.length,0);assert.match(none.notes.join(' '),/Remove it under Saved SSH connection, Other LAN addresses\./);assert.doesNotMatch(none.notes.join(' '),/address below/);
  // Two conflicts and a row with a button keep the original wording.
  const usable=discoveryView({found:[{host:'192.0.2.40',adapter:'Wi-Fi',medium:'wifi',configured_index:null}],configured:[{index:0,host:'192.0.2.41',status:'other_host'},{index:1,host:'192.0.2.20',status:'other_host'}],other_ssh_hosts:2},config);
  assert.match(usable.notes.join(' '),/Using an address below checks again and removes them from your list\./);
  const removeBoth=discoveryView({found:[],configured:[{index:0,host:'192.0.2.41',status:'other_host'},{index:1,host:'192.0.2.20',status:'other_host'}],login_rejected:1,other_ssh_hosts:2},config);
  assert.match(removeBoth.notes.join(' '),/Remove them under Saved SSH connection/);
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

test('Spark link: slow 2.4 GHz Wi-Fi warns, wired and missing reports do not', async () => {
  const {sparkLink} = await import('../ui/model.mjs');
  const obs = network => ({devices: [{id: 'spark', network}]});
  const wifi = sparkLink(obs({v: 1, observed: true, basis: 'ssh_session', active: {interface: 'wlP9s9', kind: 'wifi'}, wired_lan_up: 0, wifi: {band: '2.4', rx_bitrate_mbps: 72.2, tx_bitrate_mbps: 65}}));
  assert.equal(wifi.slow, true); assert.match(wifi.label, /Wi-Fi 2\.4 GHz · 65 Mbps/); assert.match(wifi.warning, /network cable/);
  const wired = sparkLink(obs({v: 1, observed: true, active: {interface: 'enP7s7', kind: 'wired', speed_mbps: 10000}, wired_lan_up: 1, wifi: null}));
  assert.equal(wired.slow, false); assert.equal(wired.label, 'Wired · 10 Gbps'); assert.equal(wired.warning, '');
  assert.equal(sparkLink(obs(null)), null); assert.equal(sparkLink(obs({v: 2})), null);
  assert.equal(sparkLink(obs({v: 1, observed: false, basis: 'unknown'})).warning, '');
});

test('stale status is reconnecting briefly, never connected, and authentication failures stop immediately',()=>{
  const health={failures:1,lastGoodAt:Date.now()/1000};
  assert.equal(connectionView(lan,{},false,health).state,'reconnecting');assert.equal(connectionView(lan,{},false,health).connected,false);
  assert.equal(connectionView(lan,{},false,{...health,failures:2}).state,'error');
  assert.equal(connectionView({error:{code:7,message:'pin mismatch'}},{},false,health).state,'error');
  const attempts={transport:{attempted_routes:[{route:'lan_ssh',attempted:true,status:'host_unreachable'},{route:'paired_iroh',attempted:true,status:'authentication_failed'}]}};
  const reason=plainReason({spark:attempts});assert.match(reason.text,/stopped to keep you safe/);assert.equal(reason.action,null);
});
test('internet and saved downloads distinguish old bridges, saved settings, unavailable tools and queued cleanup',()=>{
  assert.equal(internetView(null).known,false);assert.equal(internetView({v:2,enabled:true,available:true}).known,false);
  assert.equal(internetView({enabled:true,available:false}).enabled,true);assert.match(internetView({enabled:true,available:false}).label,/unavailable/);
  assert.equal(internetView({enabled:false,available:true}).label,'Off');assert.equal(jobInternet({internet:'cut_by_owner'}),'Turned off by owner');
  assert.equal(downloadsView({v:1,used_bytes:null}).label,'Measuring…');assert.equal(downloadsView({v:1,used_bytes:0}).label,'No saved downloads');
  assert.match(downloadsView({v:1,used_bytes:1024,clear_pending:true}).note,/queued/);assert.match(downloadsView({v:1,free_bytes:1,floor_bytes:2}).note,/Low disk space/);
});
test('named services stay distinct from slow batch jobs, and a queue does not claim memory as its only cause',()=>{
  const job={job:'j1',kind:'service',name:'speech',state:'running',service_state:'ready',created_at:1,started_at:2,idle_seconds:300};
  assert.equal(serviceView(job).state,'Ready');assert.match(serviceView(job).note,/5 minutes/);assert.equal(jobFlags(job,90000).some(f=>f.label==='Active for a long time'),false);
  assert.equal(serviceView({...job,state:'queued',service_state:'waiting for memory'}).state,'Waiting');
  assert.match(serviceView({...job,state:'cancelled',stop_reason:'manual'}).reason,/request/);
});
test('Spark and laptop link rates keep distinct provenance and actionable cable warnings',()=>{
  const observation={devices:[{id:'spark',transport:{route:'lan_ssh',authenticated:true,lan_path:{kind:'wifi',speed_mbps:72,faster_unanswered:true}}},{id:'laptop',network:{available:true,route_known:true,interface:{kind:'wired',transmit_link_speed_mbps:2500},speed_is_link_rate:true}}]};
  assert.match(sparkLink(observation).warning,/did not answer/);assert.equal(sparkLink(observation).action,'find');
  assert.match(laptopLink(observation).label,/Wired.*2.5 Gbps link rate/);assert.match(laptopLink(observation).note,/not measured upload speed/);
  observation.devices[0].transport.lan_path={kind:'wifi',cable_elsewhere:true};assert.match(sparkLink(observation).warning,/different network/);
  assert.equal(laptopLink({}).label,'Not reported');
  observation.devices[1].network.interface={kind:'wired',transmit_link_speed_mbps:100,usb_adapter_hint:true};observation.devices[1].network.usb_advice='Windows does not report the negotiated USB generation.';assert.match(laptopLink(observation).label,/USB Ethernet.*100 Mbps/);assert.equal(laptopLink(observation).slow,true);assert.match(laptopLink(observation).note,/may limit uploads/);assert.match(laptopLink(observation).note,/does not report/);
  observation.devices[1].network.interface.transmit_link_speed_mbps=2500;assert.equal(laptopLink(observation).slow,false);assert.doesNotMatch(laptopLink(observation).note,/may limit uploads/);
  const found=discoveryView({found:[{host:'192.0.2.41',adapter:'Laptop Wi-Fi',medium:'wifi',spark_kind:'wired',speed_mbps:1000,preselected:true},{host:'192.0.2.42',adapter:'Laptop Wi-Fi',medium:'wifi',spark_kind:'wifi',speed_mbps:72,preselected:false}]});assert.equal(found.rows[0].recommended,true);assert.match(found.rows[0].detail,/laptop's Laptop Wi-Fi.*Spark wired/);assert.match(found.summary,/recent authenticated report/);assert.doesNotMatch(found.summary,/cannot yet tell/);
});
test('agent prompts are compact, safe snapshots and cover real internet, cache and service contracts',()=>{
  for(const agent of ['codex','claude']){
    const text=agentPrompt({agent,path:'C:/Program Files/DGXBridge/bin/sparkdev.exe',stateRoot:'C:/Users/Example/AppData/Local/SparkDevBridge',config:{hosts:['192.0.2.10'],identity_file:'C:/keys/spark',connection_mode:'both'},limits:valid});
    assert.ok(text.length<5000);assert.match(text,/Snapshot from when this was copied/);assert.match(text,/Use a NEW ID to rerun a failed job/);assert.match(text,/materializing/);assert.match(text,/--no-cache reruns work but keeps saved downloads/);assert.match(text,/preparable_runtimes/);assert.match(text,/call NAME --input FILE --request-id CALL_ID/);assert.doesNotMatch(text,/Wi-Fi|2\.4 GHz/);
  }
});

test('returning work stays active and agent prompts explain automatic delivery',()=>{
  assert.equal(isActiveJob({state:'returning'}),true);
  assert.match(jobOutcome({state:'returning'}).label,/Returning/i);
  for(const agent of ['codex','claude']) {
    const text=agentPrompt({agent,config:{}});
    assert.match(text,/return automatically/);assert.match(text,/No fetch command is needed/);
    assert.match(text,/--no-return/);assert.match(text,/--runtime cuda/);
  }
});

test('named jobs keep their computer identity without changing Spark and laptop labels',()=>{
  const legacy=[{job:'same-id',where:'spark',state:'running',reserved_cpu:2},{job:'laptop-old',where:'laptop',state:'running'}];
  const named={job:'office:same-id',device:'office',device_label:'Office workstation',where:'spark',state:'returning',reserved_cpu:8};
  assert.equal(jobRunner(named),'office');assert.equal(jobRunnerLabel(named),'Office workstation');
  assert.equal(jobToolsLabel({...named,runtime:'cuda'}),'GPU tools');assert.equal(jobToolsLabel({...legacy[0],runtime:'cuda'}),'Spark GPU');assert.equal(jobToolsLabel({...named,runtime:'python'}),'Python tools');
  assert.equal(jobRunnerLabel(legacy[0]),'Spark');assert.equal(jobRunnerLabel(legacy[1]),'Laptop');assert.equal(jobRunnerLabel(legacy[1],true),'This laptop');
  const jobs=jobsFrom({shared_board:[...legacy,named]});assert.equal(jobs.length,3);
  assert.deepEqual(filterJobs(jobs,{filter:'spark'}).map(job=>job.job),['same-id']);
  assert.deepEqual(filterJobs(jobs,{filter:'active',device:'office'}).map(job=>job.job),['office:same-id']);
  assert.deepEqual(filterJobs(jobs,{query:'Office workstation'}).map(job=>job.job),['office:same-id']);
  const summary=boardSummary({devices:[{id:'spark',managed_limits:{cpu_total:16}}]},jobs);
  assert.equal(summary.sparkRunning,1);assert.equal(summary.laptopActive,1);assert.equal(summary.remoteActive,1);assert.equal(summary.reservedCpu,2);
});

test('failed named observations retain only their qualified last-seen jobs and fresh facts win',()=>{
  const previous=[{job:'same',where:'spark',state:'running'},{job:'office:same',device:'office',where:'spark',state:'running'},{job:'healthy:removed',device:'healthy',where:'spark',state:'running'},{job:'raw-unqualified',device:'office',where:'spark',state:'running'}];
  for(const failed of [{id:'office',reachable:null,availability:'observation_limited',error:{kind:'local_metadata_limit'}},{id:'office',reachable:false,error:{message:'Offline'}}]){
    const observation={devices:[{id:'spark',reachable:true},failed,{id:'healthy',reachable:true}],shared_board:[{job:'same',where:'spark',state:'succeeded',exit_code:0}]};
    const merged=reconcileObservedJobs(previous,observation);assert.deepEqual(merged.map(job=>job.job),['office:same','same']);assert.equal(merged[0].ui_observation_stale,true);assert.equal(merged[1].state,'succeeded');assert.equal(previous[1].ui_observation_stale,undefined);
    observation.shared_board.push({job:'office:same',device:'office',where:'spark',state:'succeeded',exit_code:0});const fresh=reconcileObservedJobs(merged,observation).find(job=>job.job==='office:same');assert.equal(fresh.state,'succeeded');assert.equal(fresh.ui_observation_stale,undefined);
    observation.devices[1]={id:'office',reachable:true};observation.shared_board=[];assert.deepEqual(reconcileObservedJobs(merged,observation),[]);
  }
  const baseline={devices:[{id:'spark',reachable:true}],shared_board:[previous[0]]};assert.deepEqual(reconcileObservedJobs(previous,baseline),jobsFrom(baseline));
});

test('computer cards require authenticated observations and preserve disabled profiles',()=>{
  assert.deepEqual(namedComputers(lan),[]);
  const nodes=[{id:'office',label:'Office workstation',enabled:true,priority:3,endpoint:{kind:'windows-wsl',distribution:'Ubuntu-24.04'}},{id:'render',label:'Render box',enabled:false}];
  const observations={devices:[...lan.devices,{id:'office',reachable:true,transport:{authenticated:false},hardware:{cpu_count:32,memory_gb:256}},{id:'render',reachable:true,transport:{authenticated:true},scheduler:{healthy:true}}]};
  const rows=namedComputers(observations,nodes);assert.equal(rows.length,2);assert.equal(rows[0].connected,false);assert.notEqual(rows[0].status,'Ready for jobs');assert.equal(rows[1].status,'Disabled');
  assert.equal(namedComputers(observations,nodes,{stale:true})[1].status,'Disabled');
  observations.devices[1].transport.authenticated=true;observations.devices[1].scheduler={healthy:true};observations.devices[1].runtimes=['python'];
  assert.equal(namedComputers(observations,nodes)[0].status,'Ready for jobs');
  observations.devices[1].reachable=false;observations.devices[1].availability='unreachable';observations.devices[1].error={message:'Computer is offline'};
  assert.equal(namedComputers(observations,nodes)[0].status,'Offline');assert.equal(connectionView(observations).connected,true);
  observations.devices[1].reachable=null;observations.devices[1].availability='observation_limited';observations.devices[1].error={kind:'local_metadata_limit',message:'local_metadata_limit: over 40 MiB'};
  const limited=namedComputers(observations,nodes)[0];assert.equal(limited.status,'Details unavailable');assert.equal(limited.connected,false);assert.match(limited.error,/job history is too large/);assert.match(limited.error,/does not indicate a network connection failure/);assert.doesNotMatch(limited.error,/local_metadata_limit/);
});

const KEY='ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKc3abcdefghijklmnopqrstuvwxyz0123456789ab dgxbridge-office';
const fleet={devices:[
  {id:'laptop',logical_parallelism:22},
  {id:'spark',reachable:true,transport:{authenticated:true,route:'lan_ssh'},scheduler:{healthy:true},hardware:{cpu_count:20,memory_gb:119},capabilities:{gpu:{observed:true,memory_model:'unified',devices:[{name:'NVIDIA GB10'}]},system_memory:{total_bytes:128526278656}}},
  {id:'office',label:'Office workstation',reachable:true,transport:{authenticated:true},scheduler:{healthy:true},runtimes:['cuda'],hardware:{cpu_count:64,memory_gb:125},capabilities:{gpu:{observed:true,memory_model:'discrete',devices:[{name:'NVIDIA GeForce RTX 4090',total_memory_bytes:25757220864}]}}},
]};
const officeNode=[{id:'office',label:'Office workstation',enabled:true,endpoint:{kind:'windows-wsl',distribution:'Ubuntu-20.04'}}];
test('every device has one shape: laptop, Spark and added computers, in that order',()=>{
  const rows=deviceRows({observation:fleet,nodes:officeNode,jobs:[],connection:{connected:true,enabled:true,state:'connected',route:'Same network'},config:{ssh_alias:'tuningspork-spark'}});
  assert.deepEqual(rows.map(row=>[row.id,row.kind,row.status.kind]),[['laptop','laptop','ready'],['spark','spark','ready'],['office','computer','ready']]);
  assert.match(rows[1].specs.join(' '),/GB10 GPU/);assert.match(rows[1].specs.join(' '),/shared with the GPU/);
  assert.match(rows[2].goodFor,/fit in 24 GB/);assert.match(rows[2].specs.join(' '),/RTX 4090 · 24 GB/);assert.match(rows[2].specs.join(' '),/Linux via WSL \(Ubuntu-20\.04\)/);
  assert.ok(rows[2].facts.some(([label])=>label==='GPU'));
  assert.deepEqual(fleetSummary(rows),{kind:'ready',label:'3 devices ready'});
});
test('device status reflects jobs, pauses, outages and a Spark that is not set up',()=>{
  const jobs=[{job:'office:a',device:'office',state:'running'},{job:'office:b',device:'office',state:'queued'},{job:'s1',where:'spark',state:'queued'}];
  let rows=deviceRows({observation:fleet,nodes:officeNode,jobs,connection:{connected:true,enabled:true,state:'connected'},config:{}});
  assert.deepEqual(rows[2].status,{kind:'busy',label:'1 running · 1 queued'});assert.deepEqual(rows[1].status,{kind:'ready',label:'1 queued'});
  rows=deviceRows({observation:fleet,nodes:[{...officeNode[0],enabled:false}],connection:{connected:false,enabled:false,state:'disconnected'},config:{}});
  assert.equal(rows[1].status.kind,'paused');assert.equal(rows[2].status.kind,'paused');
  rows=deviceRows({observation:fleet,nodes:officeNode,connection:{connected:false,enabled:true,state:'unavailable'},config:null});
  assert.equal(rows[1].status.kind,'setup');assert.deepEqual(fleetSummary(rows),{kind:'setup',label:'Spark: finish setup'});
  const offline=structuredClone(fleet);Object.assign(offline.devices[2],{reachable:false,availability:'unreachable',error:{message:'Computer is offline'}});
  rows=deviceRows({observation:offline,nodes:officeNode,connection:{connected:true,enabled:true,state:'connected'},config:{}});
  assert.equal(rows[2].status.kind,'offline');assert.deepEqual(fleetSummary(rows),{kind:'attention',label:'Office workstation: offline'});
});
test('device IDs agents type come from the name and never collide with reserved ones',()=>{
  assert.equal(deviceSlug('AntonPC'),'antonpc');assert.equal(deviceSlug('My Office PC!'),'my-office-pc');assert.equal(deviceSlug('2nd box'),'pc-2nd-box');
  assert.equal(deviceSlug('AntonPC',['antonpc']),'antonpc-2');assert.equal(deviceSlug('Spark'),'spark-2');assert.equal(deviceSlug('***'),'device');
  assert.ok(/^[a-z][a-z0-9-]{0,31}$/.test(deviceSlug('A'.repeat(80))));
});
test('the setup command authorizes exactly this device key and prints what the wizard asks for',()=>{
  const windows=deviceSetupCommand('windows',KEY);
  assert.match(windows,/Add-WindowsCapability -Online -Name OpenSSH\.Server/);assert.ok(windows.includes(`$k='${KEY}'`));
  assert.match(windows,/administrators_authorized_keys/);assert.match(windows,/Account: \$env:USERNAME/);assert.match(windows,/ssh_host_ed25519_key\.pub/);
  const linux=deviceSetupCommand('linux',KEY);assert.ok(linux.includes(`echo '${KEY}' >> ~/.ssh/authorized_keys`));assert.match(linux,/Account: \$\(whoami\)/);
  for(const bad of ['',"ssh-ed25519 AAAA' ; rm -rf ~ dgxbridge-x",'ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ dgxbridge-x',KEY+'\nmore'])assert.throws(()=>deviceSetupCommand('windows',bad),/key is not ready/);
  assert.throws(()=>deviceSetupCommand('mac',KEY),/Windows or Linux/);
});
test('a device check is summarized only after this laptop could sign in',()=>{
  assert.equal(probeSummary({login:'refused'}),null);
  const windows=probeSummary({login:'ok',os:'windows',hostname:'ANTONPC',address:'192.0.2.9',gpus:[{name:'NVIDIA GeForce RTX 4090',memory_gib:24}],wsl:[{distribution:'Ubuntu-20.04',user:'ben'}]});
  assert.deepEqual(windows.facts,['Windows','NVIDIA GeForce RTX 4090 · 24 GB','Linux via WSL: Ubuntu-20.04']);assert.equal(windows.needsWsl,false);assert.equal(windows.hasGpu,true);assert.deepEqual(windows.wsl,{distribution:'Ubuntu-20.04',user:'ben'});
  assert.equal(probeSummary({login:'ok',os:'windows',wsl:[]}).needsWsl,true);
  assert.deepEqual(probeSummary({login:'ok',os:'linux',architecture:'x86_64',name:'box'}).facts,['Linux (x86_64)']);
});

test('additional-computer agent instructions are additive and retain qualified job IDs',()=>{
  const baseline=agentPrompt({agent:'codex',config:{}});assert.doesNotMatch(baseline,/COMPUTERS:/);
  const text=agentPrompt({agent:'codex',config:{},computers:[{id:'office',label:'Office workstation',enabled:true},{id:'render',label:'Render workstation',enabled:false}]});
  assert.match(text,/COMPUTERS: office, render \(disabled\)/);assert.match(text,/--device ID/);assert.match(text,/--device auto/);assert.match(text,/Retain node:JOB IDs exactly/);assert.match(text,/an eligible saved computer/);assert.ok(text.length<5000);
});

test('named computer readiness requires installed runtime observations',()=>{
  const device={id:'office',reachable:true,transport:{authenticated:true},scheduler:{healthy:true}};
  const observation={devices:[device]};
  assert.equal(namedComputers(observation)[0].status,'Setup needed');
  device.runtimes=[];assert.equal(namedComputers(observation)[0].status,'Tools preparing');
  device.runtimes=[null,''];assert.notEqual(namedComputers(observation)[0].status,'Ready for jobs');
  device.runtimes=['python'];const ready=namedComputers(observation)[0];assert.equal(ready.status,'Ready for jobs');
  const stale=namedComputers(observation,[],{stale:true})[0];assert.equal(stale.status,'Last seen');assert.equal(stale.connected,false);assert.deepEqual(stale.facts.slice(1),ready.facts.slice(1));assert.equal(namedComputers(observation)[0].status,'Ready for jobs');
  device.scheduler.healthy=false;assert.equal(namedComputers(observation)[0].status,'Connected');
});

test('profile agent instructions require each remote scheduler independently',()=>{
  const baseline=agentPrompt({agent:'codex',config:{}});assert.match(baseline,/Both managed runners need the Spark scheduler/);
  const observation={devices:[{id:'spark',reachable:false,error:{message:'Spark is offline'}},{id:'office',label:'Office workstation',reachable:true,transport:{authenticated:true},scheduler:{healthy:true},runtimes:['python']}]};
  assert.equal(namedComputers(observation)[0].status,'Ready for jobs');assert.equal(connectionView(observation).connected,false);
  const text=agentPrompt({agent:'codex',config:{},connection:connectionView(observation),computers:namedComputers(observation)});
  assert.match(text,/Each selected remote computer needs its own healthy scheduler/);assert.match(text,/Native laptop admission still needs the Spark scheduler/);assert.doesNotMatch(text,/Both managed runners need the Spark scheduler/);
});

test('computer cards distinguish discrete GPU memory from system RAM',()=>{
  const observation={devices:[{id:'office',hardware:{memory_gb:256},capabilities:{gpu:{observed:true,memory_model:'discrete',devices:[{name:'GPU fixture',total_memory_bytes:24*1024**3}]}}}]};
  const facts=namedComputers(observation)[0].facts;assert.deepEqual(facts.find(([label])=>label==='System memory'),['System memory','256 GiB']);assert.match(facts.find(([label])=>label==='GPU memory')[1],/^24.*dedicated$/);
  observation.devices[0].capabilities.gpu={observed:false,memory_model:'unknown',devices:[]};assert.equal(namedComputers(observation)[0].facts.some(([label])=>label==='GPU memory'),false);
});

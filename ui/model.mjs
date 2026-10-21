// Presentation and copy helpers; never include complete backend objects in agent prompts.
export const MODE_LABELS = Object.freeze({'lan-only':'Same network only','remote-only':'Remote only',both:'Automatic'});
export const TOOL_LABELS = Object.freeze({rust:'Rust tools',node:'Node.js tools',python:'Python tools',cuda:'Spark GPU'});
export const RESOURCE_FIELDS = Object.freeze([
  ['cpu_per_job','CPU cores per job','cores'], ['memory_gb_per_job','Memory per job','GiB'],
  ['cpu_total','Total CPU budget','cores'], ['memory_gb_total','Total memory budget','GiB'],
  ['client_jobs','Concurrent jobs per agent','jobs'], ['client_cpu','CPU budget per agent','cores'],
  ['headroom_gb','Memory kept free for your Spark','GiB'],
]);
export function configMode(config) {
  return Object.hasOwn(MODE_LABELS, config?.connection_mode) ? config.connection_mode : config?.allow_remote ? 'both' : 'lan-only';
}
export function plainReason({spark,observation,mode='both',enabled=true}={}) {
  const transport=spark?.transport||observation?.transport||{},error=spark?.error||observation?.error||{};
  const attempts=Array.isArray(transport.attempted_routes)?transport.attempted_routes:[];
  const raw=error.message||transport.reason||'',codes=[error.kind,error.code,...attempts.map(a=>a.status)].map(String);
  const has=(...values)=>values.some(v=>codes.includes(v));
  if(!enabled)return {text:'Paused. Choose Connect to resume.',action:'connect',raw};
  if(has('authentication_failed')||error.code===7)return {text:"This laptop couldn't sign in, or the device answering isn't the Spark it remembers. DGXBridge stopped to keep you safe.",action:null,raw};
  if(has('authenticated_controller_failed'))return {text:"Connected, but DGXBridge on the Spark isn't responding properly.",action:'repair',raw};
  if(has('protocol_error'))return {text:"Your Spark's DGXBridge needs an update.",action:'repair',raw};
  if(has('configuration_unavailable'))return {text:'Set up your Spark connection to start.',action:'setup',raw};
  if(mode!=='remote-only'&&lanUnreachable(transport,raw))return {text:"Your Spark isn't answering on this network."+(has('unconfigured','remote_adapter_unconfigured')?' Pair this laptop to reach it away from home.':''),action:'find',raw};
  if(has('transport_timeout')||/timed out|timeout/i.test(raw))return {text:'The Spark took too long to answer. Your request may still have gone through.',action:null,raw};
  if(mode==='remote-only'&&has('unconfigured','remote_adapter_unconfigured'))return {text:"This laptop isn't paired for remote access yet.",action:'pair',raw};
  if(has('busy'))return {text:'The remote link is busy. Trying again shortly.',action:null,raw};
  if(has('remote_unavailable','transport_error','transport_unavailable'))return {text:"The remote link isn't reachable right now.",action:null,raw};
  if(has('disabled'))return {text:`The ${MODE_LABELS[mode]} setting does not allow this connection.`,action:null,raw};
  return {text:"Can't reach your Spark right now.",action:null,raw};
}
export function plainError(error) {
  const raw=String(error?.message||error||'The request did not complete.');
  if(['authentication_failed','authenticated_controller_failed','protocol_error','transport_timeout','transport_unavailable','remote_adapter_unconfigured'].includes(error?.kind)||error?.code===7)return plainReason({observation:{error:{message:raw,code:error.code,kind:error.kind}}}).text;
  return raw;
}
export function connectionView(observation, config = null, connecting = false, health = {}) {
  const spark = observation?.devices?.find(device => device.id === 'spark');
  const transport = spark?.transport || observation?.transport || {};
  const enabled = config?.connection_enabled !== false;
  const reachable = spark ? spark.reachable === true : observation?.ok === true && observation?.remote_available !== false;
  const authenticated = transport.authenticated === true;
  const mode=configMode(config);
  const mismatch=(mode==='remote-only'&&transport.route==='lan_ssh')||(mode==='lan-only'&&transport.route==='paired_iroh');
  const connected = enabled && reachable && authenticated && !mismatch && ['lan_ssh','paired_iroh'].includes(transport.route);
  const reason=plainReason({spark,observation,mode,enabled});
  const reconnecting=enabled&&!mismatch&&!connecting&&health.failures===1&&Date.now()/1000-(health.lastGoodAt||0)<30;
  const authFailed=(spark?.error?.code===7||observation?.error?.code===7)||[spark?.error?.kind,observation?.error?.kind,...(transport.attempted_routes||[]).map(a=>a.status)].includes('authentication_failed');
  const stale=health.failures>0;
  const state = !enabled ? 'disconnected' : connecting ? 'connecting' : stale&&!authFailed&&reconnecting ? 'reconnecting' : connected&&!stale ? 'connected' : mismatch || spark?.error || observation?.error || stale ? 'error' : 'unavailable';
  const path=connected&&state==='connected'?(transport.route==='lan_ssh'?'lan':'remote'):null;
  const route = path==='lan'?'Same network':path==='remote'?'Remote link':state==='disconnected'?'Paused':['connecting','reconnecting'].includes(state)?'Checking…':'Not connected';
  const message = spark?.error?.message || observation?.error?.message || transport.reason || '';
  return {state, connected:connected&&!stale, enabled, route,routeCode:transport.route||null,path,stale,mode,action:reason.action,details:message?[['Reported',message]]:[],reason: mismatch?'Your connection setting changed. Checking again…':reason.text, lanUnreachable: lanUnreachable(transport, message), schedulerHealthy:spark?.scheduler?.healthy === true || observation?.scheduler?.healthy === true};
}
// Whether no LAN address answered in the last observation. In Both mode the CLI
// goes on to the paired route and reports that route's result, so the LAN
// attempts decide: every dial and precheck found nothing answering, and none
// authenticated, dropped or failed otherwise.
function lanUnreachable(transport, message) {
  const lan = (Array.isArray(transport?.attempted_routes) ? transport.attempted_routes : []).filter(attempt => attempt?.route === 'lan_ssh' && attempt.attempted === true);
  return lan.length ? lan.every(attempt => attempt.status === 'host_unreachable') : String(message).includes(LAN_UNREACHABLE);
}
export function jobsFrom(observation) {
  const local = observation?.devices?.find(device => device.id === 'laptop')?.jobs;
  const records = [...(observation?.shared_board || observation?.jobs || []), ...(Array.isArray(local) ? local : observation?.laptop_jobs || [])];
  return [...new Map(records.map(job=>[job.job || job.job_id,job])).values()].sort((a,b)=>(Number(b.created_at)||0)-(Number(a.created_at)||0));
}
export function jobPage(jobs, requested, pageSize=6) {
  const pages = Math.max(1,Math.ceil(jobs.length/pageSize));
  const page = Math.max(0,Math.min(requested,pages-1));
  const start = page*pageSize;
  return {page,pages,start,total:jobs.length,rows:jobs.slice(start,start+pageSize)};
}
export const RESOURCE_PRESETS = Object.freeze([
  {id:'ollama-first',name:'Ollama first',description:'Keep more room for local models.',cpu_per_job:4,memory_gb_per_job:8,cpu_total:8,memory_gb_total:32,client_jobs:1,client_cpu:4,headroom_gb:64,cpuFraction:.4,memoryFraction:.6},
  {id:'balanced',name:'Balanced',description:'Share your Spark with everyday builds.',cpu_per_job:8,memory_gb_per_job:16,cpu_total:16,memory_gb_total:64,client_jobs:2,client_cpu:8,headroom_gb:32,cpuFraction:.8,memoryFraction:.3},
  {id:'builds-first',name:'Builds first',description:'More room for larger builds.',cpu_per_job:16,memory_gb_per_job:32,cpu_total:18,memory_gb_total:80,client_jobs:4,client_cpu:16,headroom_gb:32,cpuFraction:.9,memoryFraction:.3},
]);
export function resourcePreset(id,hardware) {
  const preset=RESOURCE_PRESETS.find(item=>item.id===id);
  const cores=hardware?.cpu_count,memory=hardware?.memory_gb;
  if(!preset)throw new Error('Choose a known resource preset.');
  if(!Number.isSafeInteger(cores)||cores<1||!Number.isSafeInteger(memory)||memory<2)throw new Error('Read the Spark hardware before choosing a preset.');
  const cpu_total=Math.max(1,Math.min(preset.cpu_total,Math.floor(cores*preset.cpuFraction)));
  const client_cpu=Math.min(preset.client_cpu,cpu_total);
  const headroom_gb=Math.min(preset.headroom_gb,Math.max(1,Math.floor(memory*preset.memoryFraction)));
  const memory_gb_total=Math.min(preset.memory_gb_total,memory-headroom_gb);
  return {cpu_per_job:Math.min(preset.cpu_per_job,client_cpu),memory_gb_per_job:Math.min(preset.memory_gb_per_job,memory_gb_total),cpu_total,memory_gb_total,client_jobs:Math.min(preset.client_jobs,client_cpu),client_cpu,headroom_gb};
}
export function sameResourcePolicy(a,b) {
  return !!a&&!!b&&RESOURCE_FIELDS.every(([key])=>Number(a[key])===Number(b[key]));
}
export function resourcePolicy(form,hardware=null) {
  const policy={};
  for(const [key,label] of RESOURCE_FIELDS) {
    const raw=form.get(key);const value=Number(raw);
    if(raw===null || String(raw).trim()==='' || !Number.isSafeInteger(value) || value<1) throw new Error(`${label} must be a positive whole number.`);
    policy[key]=value;
  }
  if(policy.cpu_per_job>policy.client_cpu || policy.client_cpu>policy.cpu_total) throw new Error('CPU per job must fit within the agent CPU budget and total CPU budget.');
  if(policy.memory_gb_per_job>policy.memory_gb_total) throw new Error('Memory per job must fit within the total memory budget.');
  if(policy.client_jobs>64) throw new Error('Concurrent jobs per agent must be between 1 and 64.');
  if(Number.isSafeInteger(hardware?.cpu_count)&&policy.cpu_total>hardware.cpu_count) throw new Error(`Total CPU budget must fit within the Spark’s ${hardware.cpu_count} CPU cores.`);
  if(Number.isSafeInteger(hardware?.memory_gb)&&policy.memory_gb_total+policy.headroom_gb>hardware.memory_gb) throw new Error(`Total memory budget plus memory kept free must fit within the Spark’s ${hardware.memory_gb} GiB of memory.`);
  return policy;
}
const cleanReference = value => typeof value==='string' && !/[\r\n\u0000]/.test(value) ? value : '';
export function agentPaths(agent,preferences={}) {
  const selected=preferences.agent_paths?.[agent];
  return {
    path:cleanReference(selected?.cli_path)||cleanReference(agent==='codex'?preferences.agent_cli_path:preferences.cli_path)||cleanReference(preferences.cli_path)||'sparkdev',
    stateRoot:cleanReference(selected?.state_root)||cleanReference(agent==='codex'?preferences.agent_state_root:preferences.state_root)||cleanReference(preferences.state_root),
  };
}
export function agentConfiguration(agent, path, stateRoot='') {
  // JSON/TOML string escaping preserves native UNC/verbatim Windows paths.
  // Replacing their separators would turn \\?\C:\ into unusable //?/C:/.
  const command=cleanReference(path) || 'sparkdev';
  const root=cleanReference(stateRoot);
  return agent==='claude' ? JSON.stringify({mcpServers:{sparkdev:{type:'stdio',command,args:['--client','claude','mcp'],...(root?{env:{SPARKDEV_HOME:root}}:{})}}},null,2) : `[mcp_servers.sparkdev]\ncommand = ${JSON.stringify(command)}\nargs = ["--client", "codex", "mcp"]\ntool_timeout_sec = 90${root?`\n\n[mcp_servers.sparkdev.env]\nSPARKDEV_HOME = ${JSON.stringify(root)}`:''}`;
}
export function agentPrompt({agent,path,stateRoot,config,connection,limits}) {
  const client=agent==='claude'?'claude':'codex';
  const exe=cleanReference(path) || 'sparkdev';
  const references=[];
  for(const [key,label] of [['ssh_alias','SSH alias'],['host_key_alias','Pinned known-host alias'],['user','SSH user'],['identity_file','Private-key file reference'],['local_build_gate','Local Cargo build gate']]) {
    const value=cleanReference(config?.[key]);if(value)references.push(`${label}: ${value}`);
  }
  const hosts=(Array.isArray(config?.hosts)?config.hosts:[]).map(cleanReference).filter(Boolean);
  if(hosts.length)references.push(`Configured LAN addresses: ${hosts.join(', ')}`);
  if(Number.isInteger(config?.port))references.push(`SSH port: ${config.port}`);
  if(cleanReference(stateRoot))references.push(`Set SPARKDEV_HOME explicitly to this owner-state reference: ${cleanReference(stateRoot)}. Before configuring or running work, verify returned state diagnostics resolve to this physical owner state. Packaged Codex processes may redirect AppData into Codex LocalCache; stop and report that mismatch rather than creating or migrating another identity.`);
  const resourceLines=limits ? RESOURCE_FIELDS.filter(([key])=>Number.isSafeInteger(limits[key])).map(([key,label,unit])=>`${label}: ${limits[key]} ${unit}`) : ['Spark resource policy has not been read in this app session.'];
  return `Use DGXBridge for this project. You are ${client==='claude'?'Claude Code':'Codex'}; use client label ${client}.

Installed CLI: ${exe}
Connection policy: ${MODE_LABELS[configMode(config)]}. Enabled: ${config?.connection_enabled===false?'no':'yes'}.
Last observed route: ${connection?.route||'not checked'}. Snapshot from when this was copied; devices shows the live state. A paired route does not prove hotspot or relay use.
${references.join('\n')}

CHECK: --client ${client} --json devices, then status --all. Check reachability, job service, limits, preparable_runtimes, internet, downloads and upload_limits. Missing fields mean unavailable; old bridges without internet/upload limits use offline jobs and the old 512 MiB per-file / 8 GiB upload limits. Both managed runners need the Spark scheduler. Report failure; never bypass it.
${resourceLines.join('\n')}

RUN: --client ${client} --json run PROJECT_DIR --where auto --detach --request-id RETAINED_ID -- COMMAND ARGS. Save ID and exact arguments before submitting. Reuse the same ID only to recover an uncertain submission or materializing response. Use a NEW ID to rerun a failed job. Retain job ID; poll wait JOB_ID --timeout 20 and logs JOB_ID --since OFFSET, continuing from next_offset. Inspect final workload exit_code. Use --argv-json FILE for exact arguments. --no-cache reruns work but keeps saved downloads.

RESULTS: New and changed /work files return automatically to the source project; /sparkdev-output files return to results_folder. No fetch command is needed. Keep polling while state is returning; terminal wait means hash-verified local delivery. Inspect returned and return_error. Local edits stay intact and Spark conflicts land beside them. The background worker continues after this agent exits or reconnects. --no-return is the explicit opt-out. outputs JOB_ID reports quota and retention.

WHERE: Offload Linux builds, media, render, ML and Python work to Spark by default; Windows/MSVC, installers and Windows tests stay on laptop. ffmpeg and blender select the Spark GPU runtime automatically; pass --runtime cuda for GPU Python. Native laptop jobs have no enforced CPU or memory cap. Local Cargo must use the saved build gate unchanged. Do not split one compile across OS/architecture.

DEPENDENCIES: prepare PROJECT_DIR --runtime rust|node|python before offline work. Use cuda preparation only if preparable_runtimes lists it; requirements-cuda.lock excludes torch, triton and nvidia-* wheels. For system tools, resolve a sparkdev-system.lock with /sparkdev-system.py lock in a fresh scratch project using the same runtime; then prepare the target project to build its pinned derived image. See docs/CLI.md. Run remains non-root. On insufficient_disk report it and delete nothing.

INTERNET: When enabled and available, new Spark jobs get public HTTP/HTTPS; this is not proof of a successful download. Keep HTTP_PROXY/HTTPS_PROXY and supplied cache variables. DNS errors may mean a tool ignores them. Private networks and Spark addresses are blocked. If internet.guard reports observed_public_egress_blocked, the detected public address is also blocked; extra router addresses or routes may not be detected. Checks are periodic, not instant. Jobs can send data out. Don't change the owner's policy.

SAVED DOWNLOADS: Reuse HF_HOME, TORCH_HOME, XDG_CACHE_HOME and /cache/files. Use the supplied TMPDIR for temporary files, not /tmp. --no-cache does not clear downloads. Shared files can be changed by another job. Keep tokens and credentials out of shared files.

SERVICES: Use serve PROJECT_DIR --name NAME --memory 16 --request-id START_ID -- COMMAND ARGS for a reusable model server. call NAME --input FILE --request-id CALL_ID returns a retained reply; reuse the exact ID/body after an uncertain call. Inspect its state, HTTP status and process_exit_code. Use wait/logs/cancel by name or job ID; stop by name only when connected.

SHARING: Before heavy work read status --all. Inspect others' results by job ID; don't rerun them. Client labels are self-declared. Never change connection, internet, download or resource settings without owner approval. Never run sparkdev discover; Find my Spark is an owner-only action. Never print/copy keys, credentials, tickets or approval codes. Preserve unrelated files and services. Report actual receipts, separating tests, Spark runs, Windows installation and remote verification. Agent conversations belong to CodaClauda.`;
}

// Job board presentation. Every label is derived from recorded job fields; nothing is inferred
// about live hardware usage, the launching process, or whether an agent is still polling.
export const ACTIVE_STATES = Object.freeze(['queued','starting','preparing','running','returning']);
export const CLIENT_LABELS = Object.freeze({codex:'Codex',claude:'Claude Code',human:'You'});
export const isActiveJob = job => ACTIVE_STATES.includes(job?.state);
export const jobId = job => String(job?.job || job?.job_id || '');
export const clientLabel = client => CLIENT_LABELS[client] || (client ? String(client) : 'Unknown');
export function jobRunner(job) {
  const where = job?.where || job?.placement;
  return where === 'laptop' || (!where && jobId(job).startsWith('laptop-')) ? 'laptop' : 'spark';
}
export function jobOutcome(job) {
  if (job?.cache_hit) return {kind:'cached',label:'Reused result'};
  if (job?.state === 'cancelled') return {kind:'cancelled',label:'Cancelled'};
  if (isActiveJob(job)) return {kind:'pending',label:job.state==='queued'?'Waiting':job.state==='returning'?'Returning results':'Running'};
  if (Number.isInteger(job?.exit_code)) return job.exit_code === 0 ? {kind:'ok',label:'Succeeded'} : {kind:'fail',label:`Failed (exit ${job.exit_code})`};
  return job?.state === 'failed' ? {kind:'fail',label:'Failed'} : {kind:'unknown',label:'No result'};
}
export const STALE_AFTER_SECONDS = 6 * 3600;
export function duration(seconds) {
  const s = Math.max(0, Math.round(Number(seconds) || 0));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s/60)}m ${String(s%60).padStart(2,'0')}s`;
  if (s < 86400) return `${Math.floor(s/3600)}h ${String(Math.floor(s%3600/60)).padStart(2,'0')}m`;
  return `${Math.floor(s/86400)}d ${Math.floor(s%86400/3600)}h`;
}
export function relativeTime(at, now = Date.now() / 1000) {
  const t = Number(at);
  if (!Number.isFinite(t) || t <= 0) return '—';
  const delta = now - t;
  if (delta < 45) return 'just now';
  if (delta < 3600) return `${Math.round(delta/60)}m ago`;
  if (delta < 86400) return `${Math.round(delta/3600)}h ago`;
  return `${Math.round(delta/86400)}d ago`;
}
export function jobFlags(job, now = Date.now() / 1000) {
  const flags = [];
  if (job?.cache_hit) flags.push({kind:'info',label:'Reused an earlier result',detail:`Not a fresh run. Source, argv and runtime matched a previous success${job.cached_from?` (job ${String(job.cached_from).slice(0,12)})`:''}.`});
  const since = Number(job?.started_at || job?.created_at);
  if (job?.kind!=='service' && isActiveJob(job) && Number.isFinite(since) && since > 0 && now - since > STALE_AFTER_SECONDS) flags.push({kind:'warn',label:'Active for a long time',detail:`Recorded as ${job.state} for ${duration(now - since)}. Confirm an agent is still waiting on it, or cancel it.`});
  if (job?.cancel_requested && isActiveJob(job)) flags.push({kind:'warn',label:'Cancellation requested',detail:'The runner has not reported a final state yet.'});
  const error = job?.error?.message || job?.error?.code || (typeof job?.error === 'string' ? job.error : '');
  const code=job?.error?.code||error;
  const known={insufficient_disk:'Your Spark needs more free disk space. Nothing was deleted.',disk_full:'Stopped because your Spark was running out of disk space.',launch_failed:'The job could not start. Check its tools, then submit a new job.',memory_pressure:'Stopped to keep enough memory free for your Spark.',internet_off:'Stopped after internet access was turned off.'};
  if(error)flags.push({kind:'fail',label:'Job could not finish',detail:known[code]||'The runner reported an error. Open Details for its exact message.'});
  else if(Number.isInteger(job?.exit_code)&&job.exit_code!==0){const hints={153:'A file exceeded the per-file size limit.',137:`Stopped suddenly: usually it ran out of its ${job.memory_gb??'available'} GiB of memory.`,127:"Command not found in the job's tools.",125:'The job may have produced more than 256 MiB of output.'};flags.push({kind:'fail',label:`Failed (exit ${job.exit_code})`,detail:hints[job.exit_code]||'This is the command’s result, separate from whether its submission succeeded.'});}
  return flags;
}

export function serviceView(job) {
  if(job?.kind!=='service')return null;
  const terminal=!isActiveJob(job);
  const state=terminal?({cancelled:'Stopped',succeeded:'Stopped',failed:'Failed'}[job.state]||'Stopped')
    :job.service_state==='ready'?'Ready':job.service_state==='loading'?'Loading':job.state==='queued'?'Waiting':'Loading';
  const reasons={idle:'Stopped after being unused',idle_timeout:'Stopped after being unused',lifetime:'Stopped at its time limit',lifetime_expired:'Stopped at its time limit',manual:'Stopped by request',stopped:'Stopped by request',cancelled:'Stopped by request',never_ready:'The model server did not become ready in time',reply_space_full:'Stopped because saved replies filled their space',internet_off:'Stopped after internet was turned off',replaced:'Replaced by another server',memory_pressure:'Stopped to keep memory free',disk_full:'Stopped because disk space was low',launch_failed:'Could not start',crashed:'Stopped unexpectedly'};
  return {name:cleanReference(job.name)||jobId(job),state,reason:reasons[job.stop_reason]||(job.stop_reason?'A stop reason was recorded; see Details.':''),note:`Stops by itself after ${Number.isFinite(job.idle_seconds)?Math.round(job.idle_seconds/60):30} minutes unused. A later request can restart an idle stop; a manual stop stays stopped.`};
}

export const INTERNET_COPY=Object.freeze({
  title:'Internet for Spark jobs',
  note:'New jobs can download from public websites and send data out. Turning this off also cuts internet access for running jobs; turning it on applies to new jobs.',
  blocked:'Home and private network addresses, the Spark itself (including apps such as Ollama), and cloud metadata addresses are blocked.',
  limitation:'A router may have more than one public internet address. Extra addresses, or addresses used by a different internet connection, may not be detected. Network changes are checked periodically, not instantly.',
  rule:'Agents are told not to change this setting. That is an instruction, not a separate security lock.',
});
export function internetView(value) {
  const known=value&&(value.v===undefined||value.v===1)&&typeof value.enabled==='boolean'&&typeof value.available==='boolean';
  if(!known)return {known:false,enabled:null,label:'Not reported',note:'Update the Spark bridge to use internet settings for jobs.'};
  return {known:true,enabled:value.enabled,available:value.available,label:!value.enabled?'Off':value.available?'On for new jobs':'On · downloads unavailable',note:!value.enabled?'New jobs run without internet. Already running jobs lose their internet connection.':!value.available?'The Spark’s download tools are unavailable. Repair the Spark bridge to restore them.':'Public HTTP and HTTPS are allowed. A successful download has not been tested by this setting.'};
}
export function jobInternet(job) {
  return {on:'On',off:'Off',cut_by_owner:'Turned off by owner',unavailable:'Unavailable',package_registries_only:'Package downloads only'}[job?.internet]||'Not reported';
}
export function formatGiB(bytes) {return Number.isFinite(bytes)&&bytes>=0?`${(bytes/1073741824).toFixed(1)} GiB`:'Not measured';}
export function downloadsView(value) {
  if(!value||value.v!==1)return {known:false,label:'Not reported',note:'Update the Spark bridge to manage saved downloads.',facts:[]};
  const measured=Number.isFinite(value.used_bytes)&&value.used_bytes>=0,low=Number.isFinite(value.free_bytes)&&Number.isFinite(value.floor_bytes)&&value.free_bytes<value.floor_bytes;
  return {known:true,label:!measured?'Measuring…':value.used_bytes===0?'No saved downloads':`${formatGiB(value.used_bytes)} saved`,keepGiB:Number.isFinite(value.keep_bytes)?Math.round(value.keep_bytes/1073741824):null,
    note:value.clear_pending?'Clearing is queued. It starts when Spark jobs are idle.':low?`Low disk space: ${formatGiB(value.free_bytes)} free; ${formatGiB(value.floor_bytes)} must stay free.`:value.over_limit?'Above the saved-download limit. Recent files may be kept until they are safe to remove.':'Downloads are shared between jobs. Keeping them avoids downloading the same files again.',
    facts:[['Space to keep',formatGiB(value.keep_bytes)],['Spark free space',formatGiB(value.free_bytes)],['Space kept free',formatGiB(value.floor_bytes)],['Last measured',relativeTime(value.measured_at)],['Folder',cleanReference(value.folder)||'/cache']],clearPending:value.clear_pending===true};
}
export function laptopLink(observation) {
  const report=observation?.devices?.find(d=>d.id==='laptop')?.network;
  if(!report?.available)return {label:'Not reported',note:'This laptop’s network connection has not been identified.'};
  const adapter=report.interface;
  if(!report.route_known||!adapter)return {label:'Route not identified',note:'No laptop network adapter was matched to the Spark connection.'};
  const rate=Number.isFinite(adapter.transmit_link_speed_mbps)?adapter.transmit_link_speed_mbps:null;
  const slow=rate!==null&&rate>0&&rate<300;
  const note=slow?`This laptop’s network link is slow and may limit uploads. ${cleanReference(report.usb_advice)||'A faster wired adapter or Wi-Fi connection may help.'} `:'';
  return {label:`${adapter.kind==='wired'?(adapter.usb_adapter_hint?'USB Ethernet':'Wired'):adapter.kind==='wifi'?'Wi-Fi':'Network'}${rate?` · ${rate>=1000?`${rate/1000} Gbps`:`${rate} Mbps`} link rate`:''}`,slow,note:note+'This is the laptop adapter’s link rate, not measured upload speed.'};
}
export function filterJobs(jobs, {filter='all', query=''} = {}) {
  const needle = String(query).trim().toLowerCase();
  return jobs.filter(job => {
    if (filter === 'active' && !isActiveJob(job)) return false;
    if (filter === 'failed' && jobOutcome(job).kind !== 'fail') return false;
    if ((filter === 'spark' || filter === 'laptop') && jobRunner(job) !== filter) return false;
    if (!needle) return true;
    return [jobId(job), job.client, clientLabel(job.client), job.runtime, job.name, job.kind, job.state, job.request_id, jobRunner(job)].some(value => String(value || '').toLowerCase().includes(needle));
  });
}
const total = (jobs, key, fallback) => Math.round(jobs.reduce((sum, job) => sum + (Number(job[key] ?? job[fallback]) || 0), 0) * 100) / 100;
export function boardSummary(observation, jobs) {
  const spark = observation?.devices?.find(device => device.id === 'spark') || {};
  const laptop = observation?.devices?.find(device => device.id === 'laptop') || {};
  const active = jobs.filter(isActiveJob);
  const sparkRunning = active.filter(job => jobRunner(job) === 'spark' && job.state !== 'queued');
  const limits = spark.managed_limits || null;
  const heartbeat = Number(spark.scheduler?.last_heartbeat), observedAt = Number(observation?.observed_at) || Date.now() / 1000;
  return {
    sparkRunning: sparkRunning.length,
    sparkQueued: active.filter(job => jobRunner(job) === 'spark' && job.state === 'queued').length,
    laptopActive: active.filter(job => jobRunner(job) === 'laptop').length,
    laptopLimit: Number.isSafeInteger(laptop.managed_job_limit) ? laptop.managed_job_limit : null,
    laptopThreads: Number.isSafeInteger(laptop.logical_parallelism) ? laptop.logical_parallelism : null,
    reservedCpu: total(sparkRunning, 'reserved_cpu', 'cpu'),
    reservedMemory: total(sparkRunning, 'reserved_memory_gb', 'memory_gb'),
    cpuBudget: Number.isFinite(limits?.cpu_total) ? limits.cpu_total : null,
    memoryBudget: Number.isFinite(limits?.memory_gb_total) ? limits.memory_gb_total : null,
    hardware: spark.hardware || null,
    heartbeatAge: Number.isFinite(heartbeat) && heartbeat > 0 ? Math.max(0, observedAt - heartbeat) : null,
    failedRecently: jobs.filter(job => jobOutcome(job).kind === 'fail' && Number(job.created_at) > observedAt - 86400).length,
  };
}
// The dialed address comes from lan_host (or the authenticated attempt's
// host); its configured position is found by matching config.hosts, because a
// remembered, sticky or alias attempt may have no target_index. target_index
// is only used by observations that report no dialed host.
export function routeAddress(observation, config) {
  const transport = observation?.devices?.find(device => device.id === 'spark')?.transport || {};
  if (transport.route !== 'lan_ssh' || transport.authenticated !== true) return null;
  const used = [...(transport.attempted_routes || [])].reverse().find(attempt => attempt.route === 'lan_ssh' && attempt.status === 'authenticated');
  const hosts = Array.isArray(config?.hosts) ? config.hosts : [];
  const dialed = cleanReference(transport.lan_host) || cleanReference(used?.host);
  const listed = hosts.findIndex(host => typeof host === 'string' && dialed !== '' && host.toLowerCase() === dialed.toLowerCase());
  const reported = Number.isSafeInteger(used?.target_index) && used.target_index >= 0 && used.target_index < hosts.length ? used.target_index : null;
  const index = dialed ? (listed >= 0 ? listed : null) : reported;
  const address = dialed || (index !== null ? cleanReference(hosts[index]) : '') || cleanReference(config?.ssh_alias);
  return address ? {address, index, of: hosts.length} : null;
}
export function routeAddressLabel(used) {
  if (!used) return '—';
  return used.index !== null && used.of > 1 ? `${used.address} (configured address ${used.index + 1} of ${used.of})` : used.address;
}

// Find my Spark. Values from the CLI pass through cleanReference and are
// rendered with textContent. Nothing here searches or saves: the owner starts
// both from the Spark page, never a refresh, an agent or MCP.
export const LAN_UNREACHABLE = 'Configured LAN addresses are unreachable';
export const FIND_INTRO = "Checks this laptop's own network on the SSH port and confirms answers with your pinned host key. Nothing is saved until you choose an address.";
const MEDIUM_LABELS = Object.freeze({wired:'wired', wifi:'Wi-Fi'});
const IPV4 = /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/;
const positive = value => Number.isSafeInteger(value) && value > 0 ? value : 0;
const counted = (n, one, many) => `${n} ${n === 1 ? one : many}`;
export function findSparkControl({config = null, connection = null, connecting = false, busy = false} = {}) {
  const reason = !config ? 'Save your SSH connection first. Find my Spark confirms answers with its pinned host key.'
    : config.connection_enabled === false ? 'Connect first. Find my Spark does not run while new requests are paused.'
    : configMode(config) === 'remote-only' ? 'Find my Spark uses SSH on your own network. Choose Same network only or Automatic to use it.'
    : '';
  return {disabled: busy || connecting || !!reason, reason, suggested: !reason && (connection?.lanUnreachable === true || String(connection?.reason || '').includes(LAN_UNREACHABLE))};
}
const EMPTY = Object.freeze({
  no_physical_adapter: "This laptop has no wired or Wi-Fi connection to a private home or office network that Find my Spark can search. A network reached only through a Hyper-V virtual switch or a VPN does not count. Connect this laptop to your Spark's network and search again.",
  no_routed_targets: "A VPN or another network route is carrying this laptop's local traffic, so Find my Spark checked no address. Pause the VPN and search again.",
  unconfirmed: "Addresses answered on the SSH port, but none could be confirmed as your Spark with its pinned host key and SSH login. The notes below say what happened.",
  other: "Your Spark did not answer on this laptop's network. Common causes: a Wi-Fi network that keeps devices apart (a guest network or client isolation), a VPN carrying local traffic, a Spark outside this laptop's own address range (its /24 network), or a laptop connected through a Hyper-V virtual switch.",
});
export function discoveryView(result, config = null) {
  const hosts = Array.isArray(config?.hosts) ? config.hosts.map(cleanReference) : null;
  const seen = new Set(), rows = [];
  for (const row of Array.isArray(result?.found) ? result.found : []) {
    const host = cleanReference(row?.host);
    if (!IPV4.test(host) || seen.has(host)) continue;
    seen.add(host);
    // The current saved list decides the position; the result's index is
    // only used when no configuration is at hand.
    const listed = hosts ? hosts.findIndex(item => item.toLowerCase() === host) : Number.isSafeInteger(row.configured_index) ? row.configured_index : -1;
    const configuredIndex = listed >= 0 ? listed : null;
    const adapterLabel = cleanReference(row.adapter).trim().slice(0, 80) || 'network adapter', mediumLabel = MEDIUM_LABELS[row.medium] || 'network';
    rows.push({host, adapterLabel, mediumLabel, configuredIndex, alreadyFirst: configuredIndex === 0,recommended:row.preselected===true,sparkKind:['wired','wifi'].includes(row.spark_kind)?row.spark_kind:null,
      // The adapter is this laptop's own; it says nothing about the Spark's link.
      detail: `Reached through this laptop's ${adapterLabel} (${mediumLabel})${['wired','wifi'].includes(row.spark_kind)?` · Spark ${row.spark_kind==='wired'?'wired':'Wi-Fi'}${Number.isFinite(row.speed_mbps)&&row.speed_mbps>0?` (${row.speed_mbps} Mbps link rate)`:''}`:''} · ${configuredIndex === 0 ? 'already your first LAN address' : configuredIndex !== null ? `saved as LAN address ${configuredIndex + 1}` : 'not saved yet'}`});
  }
  const conflicts = (Array.isArray(result?.configured) ? result.configured : []).filter(entry => entry?.status === 'other_host').map(entry => cleanReference(entry.host)).filter(Boolean);
  const loginRejected = positive(result?.login_rejected), localKeyError = positive(result?.local_key_error), pinHint = result?.pin_hint === true;
  const others = positive(result?.other_ssh_hosts), unchecked = positive(result?.unchecked), notChecked = positive(result?.not_checked);
  const unverified = positive(result?.unverified), unknownKey = positive(result?.unknown_key_hosts), notDirect = positive(result?.not_direct);
  // Answers that reached the SSH port but were not confirmed: the Spark may be among them.
  const answered = loginRejected + localKeyError + unknownKey + unverified + notDirect + others;
  const emptyReason = rows.length ? '' : EMPTY[result?.reason] || (answered ? EMPTY.unconfirmed : EMPTY.other);
  // Only an address that is not already first has a Use this address button.
  const usable = rows.some(row => !row.alreadyFirst);
  const summary = rows.length === 1 ? `Found your Spark at ${rows[0].host}.`
    : rows.length > 1 ? `Found your Spark at ${rows.length} addresses. ${rows.some(row=>row.sparkKind)?'Spark network labels come from its recent authenticated report. The recommended address uses those link rates; choose Use this address to save it.':"The Spark's connection type is not reported for these addresses. Choose the address you want tried first."}`
    : 'Find my Spark did not find your Spark.';
  const notes = [];
  if (conflicts.length) notes.push(`Saved ${conflicts.length === 1 ? 'address' : 'addresses'} ${conflicts.join(', ')} now ${conflicts.length === 1 ? 'answers' : 'answer'} as a different SSH device. ${usable ? `Using an address below checks again and removes ${conflicts.length === 1 ? 'it' : 'them'} from your list.` : `Remove ${conflicts.length === 1 ? 'it' : 'them'} under Saved SSH connection, Other LAN addresses.`}`);
  if (loginRejected) notes.push(`${counted(loginRejected, 'address', 'addresses')} refused the SSH login. Check the SSH username and key file under Saved SSH connection.`);
  if (localKeyError) notes.push(`This laptop's SSH key or SSH settings could not be used for ${counted(localKeyError, 'address', 'addresses')}, often because of file permissions. Fix that, then search again.`);
  if (pinHint) notes.push("The answers did not match the host key this laptop has saved for your Spark, or none is saved. If you reinstalled the Spark, update its known_hosts entry yourself, then search again. DGXBridge never changes it for you.");
  else if (unknownKey) notes.push(`${counted(unknownKey, 'SSH device', 'SSH devices')} answered with a host key that does not match the one this laptop has saved for your Spark${rows.length ? ` and ${unknownKey === 1 ? 'was' : 'were'} left alone.` : '. If you reinstalled the Spark, update its known_hosts entry yourself, then search again. DGXBridge never changes it for you.'}`);
  if (unverified) notes.push(`${counted(unverified, 'address', 'addresses')} answered on the SSH port, but the pinned check broke off or did not finish in time. Search again.`);
  if (notDirect) notes.push(`${counted(notDirect, 'answering address', 'answering addresses')} would be reached through a proxy, jump host or bind option in your SSH settings, so Find my Spark did not check ${notDirect === 1 ? 'it' : 'them'}.`);
  if (others) notes.push(`${counted(others, 'other SSH device', 'other SSH devices')} answered and ${others === 1 ? 'was' : 'were'} left alone.`);
  if (unchecked) notes.push(`${counted(unchecked, 'address', 'addresses')} could not be checked in time, so this result may be incomplete.`);
  if (notChecked) notes.push(`${counted(notChecked, 'more address', 'more addresses')} answered but ${notChecked === 1 ? 'was' : 'were'} not checked; Find my Spark checks up to 32.`);
  return {rows, loginRejected, localKeyError, unverified, unknownKey, notDirect, conflicts, pinHint, emptyReason, summary, notes};
}
export function discoverySaved(result, host = '') {
  const saved = cleanReference(result?.host) || cleanReference(host);
  const removed = (Array.isArray(result?.removed) ? result.removed : []).map(cleanReference).filter(Boolean);
  const parts = [`Saved ${saved} as the first LAN address DGXBridge tries.`];
  if (removed.length) parts.push(`Removed ${removed.join(', ')}, which now ${removed.length === 1 ? 'answers' : 'answer'} as a different SSH device.`);
  const warning = cleanReference(result?.warning);
  if (warning) parts.push(warning);
  return parts.join(' ');
}
// The "Other LAN addresses" field: separated by commas or spaces, without
// repeats or the first LAN address, in the order written.
export function otherLanAddresses(text, first = '') {
  const seen = new Set([String(first || '').trim().toLowerCase()].filter(Boolean));
  return String(text ?? '').split(/[\s,;]+/).filter(value => {
    const key = value.toLowerCase();
    if (!value || seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

// The Spark's own report of the link it is using (controller status.network, v1). Plain words only;
// the report is observation, never a routing decision. Null when the Spark bridge predates the report.
export function sparkLink(observation) {
  const spark=observation?.devices?.find(device=>device.id==='spark'),path=spark?.transport?.lan_path;
  const network = spark?.network;
  if(spark?.transport?.route==='lan_ssh'&&spark.transport.authenticated===true&&path&&['wired','wifi'].includes(path.kind)){
    const rate=Number.isFinite(path.speed_mbps)&&path.speed_mbps>0?path.speed_mbps:null;
    const label=`${path.kind==='wired'?'Wired':'Wi-Fi'}${rate?` · ${rate>=1000?`${rate/1000} Gbps`:`${rate} Mbps`} link rate`:''}`;
    const noCable=network?.v===1&&network.wired_lan_up===0;
    const slow=path.kind==='wifi'&&(network?.wifi?.band==='2.4'||rate!==null&&rate<200);
    let warning='',action=null;
    if(path.faster_unanswered){warning='A faster wired connection did not answer. Using the connection that works.';action='find';}
    else if(path.kind==='wifi'&&path.cable_elsewhere)warning='Your Spark’s cable is on a different network. Connect the laptop and Spark to the same network to use it.';
    else if(path.kind==='wifi'&&path.shared_network){warning='Your Spark has a cable on this network, but this request used Wi-Fi. Check the wired connection.';action='find';}
    else if(slow&&noCable)warning='Your Spark is on slow Wi-Fi. Connect it to your router with a network cable for faster uploads.';
    return {label,slow,warning,action};
  }
  if (!network || network.v !== 1) return null;
  if (!network.observed || !network.active) return {label: 'Not reported by the Spark', slow: false, warning: ''};
  const active = network.active, wifi = network.wifi;
  if (active.kind === 'wired') return {label: `Wired${active.speed_mbps ? ` · ${active.speed_mbps >= 1000 ? active.speed_mbps / 1000 + ' Gbps' : active.speed_mbps + ' Mbps'}` : ''}`, slow: false, warning: ''};
  if (active.kind === 'wifi') {
    const rates = [wifi?.rx_bitrate_mbps, wifi?.tx_bitrate_mbps].filter(Number.isFinite);
    const rate = rates.length ? Math.min(...rates) : null;
    const band = wifi?.band && wifi.band !== 'multi' ? `${wifi.band} GHz ` : '';
    const slow = wifi?.band === '2.4' || (rate !== null && rate < 200);
    const noCable = !(network.wired_lan_up > 0);
    return {label: `Wi-Fi ${band}${rate !== null ? `· ${Math.round(rate)} Mbps` : ''}`.trim(), slow,
      warning: noCable ? (slow ? `Your Spark is on slow ${band}Wi-Fi. Connect it to your router with a network cable for faster uploads.` : '') : 'Your Spark has a cable connected, but this connection is using Wi-Fi. Check the wired connection.', action: noCable ? null : 'find'};
  }
  return {label: 'Connected', slow: false, warning: ''};
}
export const ROUTE_WORDS = Object.freeze({'Pinned SSH · LAN': 'Your home network', 'Paired connection · path unknown': 'Remote connection'});

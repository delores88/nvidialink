// Presentation and copy helpers; never include complete backend objects in agent prompts.
export const MODE_LABELS = Object.freeze({'lan-only':'LAN only','remote-only':'Remote only',both:'LAN + remote'});
export const RESOURCE_FIELDS = Object.freeze([
  ['cpu_per_job','CPU cores per job','cores'], ['memory_gb_per_job','Memory per job','GiB'],
  ['cpu_total','Total CPU budget','cores'], ['memory_gb_total','Total memory budget','GiB'],
  ['client_jobs','Concurrent jobs per agent','jobs'], ['client_cpu','CPU budget per agent','cores'],
  ['headroom_gb','Memory kept free for your Spark','GiB'],
]);
export function configMode(config) {
  return Object.hasOwn(MODE_LABELS, config?.connection_mode) ? config.connection_mode : config?.allow_remote ? 'both' : 'lan-only';
}
export function connectionView(observation, config = null, connecting = false) {
  const spark = observation?.devices?.find(device => device.id === 'spark');
  const transport = spark?.transport || observation?.transport || {};
  const enabled = config?.connection_enabled !== false;
  const reachable = spark ? spark.reachable === true : observation?.ok === true && observation?.remote_available !== false;
  const authenticated = transport.authenticated === true;
  const mode=configMode(config);
  const mismatch=(mode==='remote-only'&&transport.route==='lan_ssh')||(mode==='lan-only'&&transport.route==='paired_iroh');
  const connected = enabled && reachable && authenticated && !mismatch && ['lan_ssh','paired_iroh'].includes(transport.route);
  const state = !enabled ? 'disconnected' : connecting ? 'connecting' : connected ? 'connected' : mismatch || spark?.error || observation?.error ? 'error' : 'unavailable';
  const route = connected && transport.route === 'lan_ssh' ? 'Pinned SSH · LAN' : connected && transport.route === 'paired_iroh' ? 'Paired connection · path unknown' : state === 'disconnected' ? 'Connection paused' : state === 'connecting' ? 'Checking your saved connection' : 'No authenticated connection';
  return {state, connected, enabled, route, mode, reason: mismatch?'The last observed route differs from your saved mode. A fresh connection check is needed.':spark?.error?.message || observation?.error?.message || transport.reason || '', schedulerHealthy:spark?.scheduler?.healthy === true || observation?.scheduler?.healthy === true};
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
  if(Number.isSafeInteger(hardware?.memory_gb)&&policy.memory_gb_total+policy.headroom_gb>hardware.memory_gb) throw new Error(`Total memory budget plus headroom must fit within the Spark’s ${hardware.memory_gb} GiB of memory.`);
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
  return `Use DGXBridge for this project. You are ${client==='claude'?'Claude Code':'Codex'}; use client label ${client}.\n\nInstalled CLI: ${exe}\nConnection policy: ${MODE_LABELS[configMode(config)]}. Connection enabled: ${config?.connection_enabled===false?'no':'yes'}.\nLast observed route: ${connection?.route || 'not checked'}. This is an observation, not a reserved connection or capacity. Recheck devices before work. Paired transport does not prove a relay or hotspot path.\n${references.join('\n')}\n\nObserved Spark policy:\n${resourceLines.join('\n')}\n\nStart with the installed CLI's --client ${client} --json devices, then status --all. Use its MCP tools if configured. If the Spark or scheduler is unavailable, explain the observed failure; do not weaken host-key checks, invent capacity, or silently bypass the scheduler. Both managed runners currently require the Spark scheduler.\n\nFor a job, use --client ${client} --json run PROJECT_DIR --where auto --detach --request-id RETAINED_ID -- COMMAND ARGS. Keep one request ID for an uncertain submission and retain the returned job ID. Poll wait JOB_ID --timeout 20. Fetch logs JOB_ID --since OFFSET and continue from next_offset. Inspect final state and workload exit_code. Repeated successful Spark jobs may use the result cache; --no-cache forces a fresh run. Use --argv-json FILE when shell quoting needs exact arguments.\n\nUse Spark for Linux-capable work and laptop for Windows/MSVC, installers and Windows-only tests. Native laptop jobs have no enforced CPU or memory cap; direct Cargo must use the saved build gate unchanged. Do not split one compile across OS/architecture. Run prepare PROJECT_DIR --runtime rust|node|python for locked dependencies before offline jobs. Do not modify unrelated services or containers. Never print or copy private keys, credential contents, pairing tickets or approval codes.\n\nThe job board is shared with the other agent and the owner. Before heavy work, read status --all and avoid stacking onto a runner that another agent is already saturating. Another agent's result can be inspected by its job ID with wait and logs; never resubmit its work to read a result. Client labels are self-declared, so attribute jobs only to what you actually submitted.\n\nAsk before changing the owner's connection or resource policy. Report actual receipts and distinguish source tests, Spark jobs, installed Windows checks and remote-network verification. DGXBridge coordinates workloads; separate agent conversations belong to CodaClauda.`;
}

// Job board presentation. Every label is derived from recorded job fields; nothing is inferred
// about live hardware usage, the launching process, or whether an agent is still polling.
export const ACTIVE_STATES = Object.freeze(['queued','starting','preparing','running']);
export const CLIENT_LABELS = Object.freeze({codex:'Codex',claude:'Claude Code',human:'You'});
export const isActiveJob = job => ACTIVE_STATES.includes(job?.state);
export const jobId = job => String(job?.job || job?.job_id || '');
export const clientLabel = client => CLIENT_LABELS[client] || (client ? String(client) : 'Unknown');
export function jobRunner(job) {
  const where = job?.where || job?.placement;
  return where === 'laptop' || (!where && jobId(job).startsWith('laptop-')) ? 'laptop' : 'spark';
}
export function jobOutcome(job) {
  if (job?.cache_hit) return {kind:'cached',label:'Cached result'};
  if (job?.state === 'cancelled') return {kind:'cancelled',label:'Cancelled'};
  if (isActiveJob(job)) return {kind:'pending',label:job.state==='queued'?'Waiting':'In progress'};
  if (Number.isInteger(job?.exit_code)) return job.exit_code === 0 ? {kind:'ok',label:'Exit 0'} : {kind:'fail',label:`Exit ${job.exit_code}`};
  return job?.state === 'failed' ? {kind:'fail',label:'Failed'} : {kind:'unknown',label:'No exit code'};
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
  if (isActiveJob(job) && Number.isFinite(since) && since > 0 && now - since > STALE_AFTER_SECONDS) flags.push({kind:'warn',label:'Active for a long time',detail:`Recorded as ${job.state} for ${duration(now - since)}. Confirm an agent is still waiting on it, or cancel it.`});
  if (job?.cancel_requested && isActiveJob(job)) flags.push({kind:'warn',label:'Cancellation requested',detail:'The runner has not reported a final state yet.'});
  if (Number.isInteger(job?.exit_code) && job.exit_code !== 0) flags.push({kind:'fail',label:`Workload exited with ${job.exit_code}`,detail:'The submitting CLI call can still report ok:true. This exit code is the workload result.'});
  const error = job?.error?.message || (typeof job?.error === 'string' ? job.error : '');
  if (error) flags.push({kind:'fail',label:'Runner error',detail:error});
  return flags;
}
export function filterJobs(jobs, {filter='all', query=''} = {}) {
  const needle = String(query).trim().toLowerCase();
  return jobs.filter(job => {
    if (filter === 'active' && !isActiveJob(job)) return false;
    if (filter === 'failed' && jobOutcome(job).kind !== 'fail') return false;
    if ((filter === 'spark' || filter === 'laptop') && jobRunner(job) !== filter) return false;
    if (!needle) return true;
    return [jobId(job), job.client, clientLabel(job.client), job.runtime, job.state, job.request_id, jobRunner(job)].some(value => String(value || '').toLowerCase().includes(needle));
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

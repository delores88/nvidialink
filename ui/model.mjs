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
export function resourcePolicy(form) {
  const policy={};
  for(const [key,label] of RESOURCE_FIELDS) {
    const raw=form.get(key);const value=Number(raw);
    if(raw===null || String(raw).trim()==='' || !Number.isSafeInteger(value) || value<1) throw new Error(`${label} must be a positive whole number.`);
    policy[key]=value;
  }
  if(policy.cpu_per_job>policy.client_cpu || policy.client_cpu>policy.cpu_total) throw new Error('CPU per job must fit within the agent CPU budget and total CPU budget.');
  if(policy.memory_gb_per_job>policy.memory_gb_total) throw new Error('Memory per job must fit within the total memory budget.');
  if(policy.client_jobs>64) throw new Error('Concurrent jobs per agent must be between 1 and 64.');
  return policy;
}
const cleanReference = value => typeof value==='string' && !/[\r\n\u0000]/.test(value) ? value : '';
export function agentConfiguration(agent, path, stateRoot='') {
  const command=cleanReference(path).replaceAll('\\','/') || 'sparkdev';
  const root=cleanReference(stateRoot);
  return agent==='claude' ? JSON.stringify({mcpServers:{sparkdev:{type:'stdio',command,args:['--client','claude','mcp'],...(root?{env:{SPARKDEV_HOME:root}}:{})}}},null,2) : `[mcp_servers.sparkdev]\ncommand = ${JSON.stringify(command)}\nargs = ["--client", "codex", "mcp"]\ntool_timeout_sec = 90${root?`\n\n[mcp_servers.sparkdev.env]\nSPARKDEV_HOME = ${JSON.stringify(root)}`:''}`;
}
export function agentPrompt({agent,path,stateRoot,config,connection,limits}) {
  const client=agent==='claude'?'claude':'codex';
  const exe=cleanReference(path).replaceAll('\\','/') || 'sparkdev';
  const references=[];
  for(const [key,label] of [['ssh_alias','SSH alias'],['host_key_alias','Pinned known-host alias'],['user','SSH user'],['identity_file','Private-key file reference'],['local_build_gate','Local Cargo build gate']]) {
    const value=cleanReference(config?.[key]);if(value)references.push(`${label}: ${value}`);
  }
  const hosts=(Array.isArray(config?.hosts)?config.hosts:[]).map(cleanReference).filter(Boolean);
  if(hosts.length)references.push(`Configured LAN addresses: ${hosts.join(', ')}`);
  if(Number.isInteger(config?.port))references.push(`SSH port: ${config.port}`);
  if(cleanReference(stateRoot))references.push(`Set SPARKDEV_HOME explicitly to this owner-state reference: ${cleanReference(stateRoot)}. Before configuring or running work, verify returned state diagnostics resolve to this physical owner state. Packaged Codex processes may redirect AppData into Codex LocalCache; stop and report that mismatch rather than creating or migrating another identity.`);
  const resourceLines=limits ? RESOURCE_FIELDS.filter(([key])=>Number.isSafeInteger(limits[key])).map(([key,label,unit])=>`${label}: ${limits[key]} ${unit}`) : ['Spark resource policy has not been read in this app session.'];
  return `Use DGXBridge for this project. You are ${client==='claude'?'Claude Code':'Codex'}; use client label ${client}.\n\nInstalled CLI: ${exe}\nConnection policy: ${MODE_LABELS[configMode(config)]}. Connection enabled: ${config?.connection_enabled===false?'no':'yes'}.\nLast observed route: ${connection?.route || 'not checked'}. This is an observation, not a reserved connection or capacity. Recheck devices before work. Paired transport does not prove a relay or hotspot path.\n${references.join('\n')}\n\nObserved Spark policy:\n${resourceLines.join('\n')}\n\nStart with the installed CLI's --client ${client} --json devices, then status --all. Use its MCP tools if configured. If the Spark or scheduler is unavailable, explain the observed failure; do not weaken host-key checks, invent capacity, or silently bypass the scheduler. Both managed runners currently require the Spark scheduler.\n\nFor a job, use --client ${client} --json run PROJECT_DIR --where auto --detach --request-id RETAINED_ID -- COMMAND ARGS. Keep one request ID for an uncertain submission and retain the returned job ID. Poll wait JOB_ID --timeout 20. Fetch logs JOB_ID --since OFFSET and continue from next_offset. Inspect final state and workload exit_code. Repeated successful Spark jobs may use the result cache; --no-cache forces a fresh run. Use --argv-json FILE when shell quoting needs exact arguments.\n\nUse Spark for Linux-capable work and laptop for Windows/MSVC, installers and Windows-only tests. Native laptop jobs have no enforced CPU or memory cap; direct Cargo must use the saved build gate unchanged. Do not split one compile across OS/architecture. Run prepare PROJECT_DIR --runtime rust|node|python for locked dependencies before offline jobs. Do not modify unrelated services or containers. Never print or copy private keys, credential contents, pairing tickets or approval codes.\n\nAsk before changing the owner's connection or resource policy. Report actual receipts and distinguish source tests, Spark jobs, installed Windows checks and remote-network verification. DGXBridge coordinates workloads; separate agent conversations belong to ClaudaCoda.`;
}

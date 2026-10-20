import {qrArt,eyeRing,eyePupil,alignRing,alignDot,sparkPath,checkPoints} from './vendor/sparkLinkQr.mjs';
import {createSim,setTargets,step,draw,isActive,placeOnCells,burst} from './vendor/sparkParticles.mjs';
import {MODE_LABELS,RESOURCE_FIELDS,RESOURCE_PRESETS,configMode,connectionView,jobsFrom,jobPage,resourcePolicy,resourcePreset,sameResourcePolicy,agentPaths,agentConfiguration,agentPrompt,
  isActiveJob,jobId,jobRunner,jobOutcome,jobFlags,filterJobs,clientLabel,duration,relativeTime,boardSummary,routeAddress,routeAddressLabel} from './model.mjs';
import {createConnectionVisual} from './connection-visual.mjs';
const $=id=>document.getElementById(id);
const invoke=(name,args={})=>window.__TAURI__.core.invoke(name,args);
const motionQuery=window.matchMedia('(prefers-reduced-motion: reduce)');
const PAGE_SIZE=8;
let reduced=motionQuery.matches,currentView='overview',refreshing=false,connecting=false;
let pairTimer=null,pairArt=null,pairCanvas=null,pairSim=null,pairFrame=0,lastPairState='';
let preferences={},agent='codex',savedConfig=null,observation=null,limits=null,laptop=null;
let resourceHardware=null,resourceDirty=false,resourceBusy=false,resourceReady=false,resourceGeneration=0;
let boardJobs=[],pageIndex=0,jobFilter='all',jobQuery='',selectedJob=null,logState=null,logTimer=null,cancelArmed=null;
let updateTimer=null,updateBusy=false,update={status:'idle'},messageTimer=null;
const visuals=[createConnectionVisual($('connection-visual'))];
function element(tag,className,text){const node=document.createElement(tag);if(className)node.className=className;if(text!=null)node.textContent=text;return node;}
function message(text){clearTimeout(messageTimer);$('message').textContent=text||'';$('message').hidden=!text;if(text)messageTimer=setTimeout(()=>{$('message').hidden=true;},7000);}
function requireOk(value){if(value?.ok===false)throw new Error(value.error?.message||value.message||'The operation did not complete.');return value;}
async function command(args){return requireOk(await invoke('cli',{args}));}
const failure=error=>String(error?.message||error||'The operation did not complete.');
async function busy(button,action){if(button.disabled)return;button.disabled=true;try{await action();}catch(error){message(failure(error));}finally{button.disabled=false;}}

// Theme follows CodaClauda: dark by default, remembered per viewer when storage is available.
function applyTheme(theme){document.documentElement.dataset.theme=theme;const light=theme==='light';$('theme-toggle').setAttribute('aria-label',light?'Switch to dark theme':'Switch to light theme');$('theme-toggle').querySelector('use').setAttribute('href',light?'#i-moon':'#i-sun');}
try{applyTheme(localStorage.getItem('dgxbridge-theme')==='light'?'light':'dark');}catch{applyTheme('dark');}
$('theme-toggle').addEventListener('click',()=>{const next=document.documentElement.dataset.theme==='light'?'dark':'light';applyTheme(next);try{localStorage.setItem('dgxbridge-theme',next);}catch{}});

const TITLES={overview:'Jobs',connection:'Spark connection',resources:'Build limits',agents:'Agent setup'};
function view(name){currentView=name;document.querySelectorAll('.view').forEach(el=>el.hidden=el.id!==name);document.querySelectorAll('.nav[data-view]').forEach(el=>{const active=el.dataset.view===name;el.classList.toggle('active',active);if(active)el.setAttribute('aria-current','page');else el.removeAttribute('aria-current');});$('page-title').textContent=TITLES[name];document.querySelector('.views').scrollTop=0;if(name==='resources'&&!limits)readResources();if(name==='connection')readPairState();}
document.querySelectorAll('[data-view]').forEach(button=>button.addEventListener('click',()=>{if(button.dataset.agentChoice){agent=button.dataset.agentChoice;renderAgents();}view(button.dataset.view);}));

// Connection
const STATE_LABELS={connected:'Spark connected',disconnected:'Disconnected',connecting:'Connecting…',error:'Needs attention',unavailable:'Spark unavailable'};
function renderConnection(){
  const state=connectionView(observation,savedConfig,connecting),spark=observation?.devices?.find(device=>device.id==='spark'),used=routeAddress(observation,savedConfig);
  const unconfigured=!savedConfig&&state.state==='error',shown=unconfigured?'unavailable':state.state,label=unconfigured?'Spark not set up':STATE_LABELS[state.state];
  document.querySelectorAll('[data-connection-state]').forEach(el=>el.textContent=label);
  document.querySelectorAll('[data-connection-dot]').forEach(el=>el.dataset.state=shown);
  document.querySelectorAll('[data-connection-link]').forEach(el=>el.dataset.state=shown);
  document.querySelectorAll('[data-mode-label]').forEach(el=>el.textContent=MODE_LABELS[state.mode]);
  document.querySelectorAll('[data-spark-name]').forEach(el=>el.textContent=savedConfig?.ssh_alias||'Your Spark');
  const routeText=state.connected?(used?`${state.route} · ${used.address}`:state.route):unconfigured?'Save an SSH connection to start':label;
  document.querySelectorAll('[data-connection-route]').forEach(el=>el.textContent=routeText);
  const note=!state.enabled?'New requests are paused. Running jobs and the saved pairing are unchanged.':state.connected?(state.schedulerHealthy?'Authenticated, and the shared scheduler is responding.':'Authenticated, but the scheduler did not report healthy. New jobs may not be admitted.'):(state.reason||'Save your SSH connection, then connect.');
  document.querySelectorAll('[data-connection-note]').forEach(el=>el.textContent=note);
  $('connection-toggle').textContent=state.enabled?'Disconnect':'Connect';$('connection-toggle').className=`button ${state.enabled?'secondary':'primary'}`;$('connection-toggle').disabled=connecting||!savedConfig;
  const transport=spark?.transport||{},heartbeat=Number(spark?.scheduler?.last_heartbeat),hardware=spark?.hardware;
  const facts=[['Route',state.connected?state.route:'None authenticated'],['Address used',routeAddressLabel(used)],['Scheduler',spark?.scheduler?(spark.scheduler.healthy?'Healthy':'Not healthy'):'Not observed'],['Heartbeat',Number.isFinite(heartbeat)&&heartbeat>0?relativeTime(heartbeat,observation?.observed_at):'—'],['Spark hardware',hardware?.cpu_count?`${hardware.cpu_count} CPU cores · ${hardware.memory_gb} GiB`:'Not observed'],['Last check',observation?.observed_at?relativeTime(observation.observed_at):'Not checked']];
  if(transport.reason&&!state.connected)facts.push(['Reason',transport.reason]);
  renderFacts($('route-facts'),facts);
  visuals.forEach(visual=>visual.render(state,reduced));renderAgents();
}
function renderFacts(list,facts){list.replaceChildren(...facts.flatMap(([term,value,className])=>{const wrap=element('div',className||'');wrap.append(element('dt',null,term),element('dd',null,value));return [wrap];}));}
async function refresh(){if(refreshing)return;refreshing=true;$('refresh').disabled=true;$('refresh').classList.add('spinning');try{observation=await command(['devices']);boardJobs=jobsFrom(observation);$('connection-detail').textContent=JSON.stringify(observation,null,2);}catch(error){observation={ok:false,error:{message:failure(error)}};$('connection-detail').textContent=failure(error);}finally{refreshing=false;$('refresh').disabled=false;$('refresh').classList.remove('spinning');renderConnection();renderBoard();}}
$('refresh').addEventListener('click',refresh);

// Jobs
function renderBoard(){
  const summary=boardSummary(observation,boardJobs),state=connectionView(observation,savedConfig,connecting),active=summary.sparkRunning+summary.sparkQueued+summary.laptopActive;
  $('activity-label').textContent=active?`${active} active ${active===1?'job':'jobs'}`:'No active jobs';$('activity-dot').dataset.active=String(active>0);
  $('nav-active').hidden=!active;$('nav-active').textContent=active>9?'9+':String(active);
  $('laptop-jobs').textContent=`${summary.laptopActive} active`;
  const slots=summary.laptopLimit;$('laptop-meter-fill').style.width=`${slots?Math.min(100,summary.laptopActive/slots*100):0}%`;$('laptop-meter').setAttribute('aria-label',slots?`${summary.laptopActive} of ${slots} laptop job slots in use`:'Laptop job slots not reported');
  $('laptop-foot').textContent=slots?`${summary.laptopActive} of ${slots} job slots in use${summary.laptopThreads?` · ${summary.laptopThreads} logical CPUs`:''} · no enforced caps`:'Native jobs run as you, without enforced CPU or memory caps.';
  $('spark-jobs').textContent=summary.sparkQueued?`${summary.sparkRunning} running · ${summary.sparkQueued} queued`:`${summary.sparkRunning} active`;
  const budget=summary.cpuBudget;$('spark-meter-fill').style.width=`${budget?Math.min(100,summary.reservedCpu/budget*100):0}%`;$('spark-meter').setAttribute('aria-label',budget?`${summary.reservedCpu} of ${budget} budgeted CPU cores reserved by running jobs`:'Spark CPU budget not observed');
  $('spark-foot').textContent=!state.connected?'Linux builds, tests and compute. Connect to see the scheduler.':budget?`Reserved ${summary.reservedCpu}/${budget} cores · ${summary.reservedMemory}/${summary.memoryBudget??'—'} GiB by running jobs${summary.heartbeatAge!=null?` · scheduler seen ${relativeTime(Number(observation.observed_at)-summary.heartbeatAge,observation.observed_at)}`:''}`:'Scheduler responding. Budgets not reported.';
  renderJobs();
}
function stateLabel(state){return state?state[0].toUpperCase()+state.slice(1):'Unknown';}
function renderJobs(){
  const filtered=filterJobs(boardJobs,{filter:jobFilter,query:jobQuery}),page=jobPage(filtered,pageIndex,PAGE_SIZE);pageIndex=page.page;
  $('job-count').textContent=String(boardJobs.length);
  const list=$('jobs');list.replaceChildren();
  $('empty-jobs').hidden=page.total>0;$('empty-jobs-text').textContent=boardJobs.length?'No jobs match this filter.':observation?.ok===false?'The job board could not be read. Check the Spark connection.':'No jobs recorded yet. Ask an agent to run a build.';
  $('pagination').hidden=page.pages<2;$('page-summary').textContent=`${page.total?page.start+1:0}–${Math.min(page.start+PAGE_SIZE,page.total)} of ${page.total} jobs`;$('page-position').textContent=`Page ${page.page+1} of ${page.pages}`;$('previous-page').disabled=page.page===0;$('next-page').disabled=page.page===page.pages-1;
  const now=Number(observation?.observed_at)||Date.now()/1000;
  for(const job of page.rows){
    const id=jobId(job),outcome=jobOutcome(job),runner=jobRunner(job),flags=jobFlags(job,now);
    const item=element('li');const button=element('button','job-row');button.type='button';button.dataset.job=id;button.dataset.outcome=outcome.kind;if(id===selectedJob)button.setAttribute('aria-current','true');
    button.setAttribute('aria-label',`Job ${id}, ${clientLabel(job.client)}, ${runner}, ${job.state||'unknown'}, ${outcome.label}`);
    const top=element('span','job-row-top');const state=element('span','job-state');state.dataset.state=job.state||'unknown';state.title=stateLabel(job.state);top.append(state,element('span','job-id',id.slice(0,12)||'—'),element('span','job-time',relativeTime(job.created_at,now)));
    const bottom=element('span','job-row-meta');const who=element('span',`client-tag client-${job.client||'unknown'}`,clientLabel(job.client));bottom.append(who,element('span','runner-tag',runner==='laptop'?'Laptop':'Spark'));if(job.runtime)bottom.append(element('span','meta-text',job.runtime));
    const result=element('span',`outcome outcome-${outcome.kind}`,outcome.label);bottom.append(result);
    if(flags.some(flag=>flag.kind==='warn')){const warn=element('span','row-warn','!');warn.title=flags.filter(flag=>flag.kind==='warn').map(flag=>flag.label).join(', ');bottom.append(warn);}
    button.append(top,bottom);button.addEventListener('click',()=>selectJob(id));item.append(button);list.append(item);
  }
  renderJobDetail();
}
$('previous-page').addEventListener('click',()=>{pageIndex--;renderJobs();});$('next-page').addEventListener('click',()=>{pageIndex++;renderJobs();});
document.querySelectorAll('[data-filter]').forEach(button=>button.addEventListener('click',()=>{jobFilter=button.dataset.filter;pageIndex=0;document.querySelectorAll('[data-filter]').forEach(el=>el.setAttribute('aria-pressed',String(el===button)));renderJobs();}));
$('job-search').addEventListener('input',event=>{jobQuery=event.target.value;pageIndex=0;renderJobs();});
document.addEventListener('keydown',event=>{if((event.ctrlKey||event.metaKey)&&event.key.toLowerCase()==='k'){event.preventDefault();view('overview');$('job-search').focus();}if(event.key==='F5'){event.preventDefault();refresh();}if(event.key==='Escape'&&selectedJob&&currentView==='overview'&&!document.querySelector('dialog[open]'))selectJob(null);});
function selectJob(id){if(selectedJob!==id){selectedJob=id;cancelArmed=null;stopLogFollow();logState=id?{job:id,text:'',offset:0,loading:false,error:''}:null;}renderJobs();if(id)loadLogs(true);}
$('close-job').addEventListener('click',()=>selectJob(null));
function renderJobDetail(){
  const job=boardJobs.find(item=>jobId(item)===selectedJob);
  $('job-welcome').hidden=!!job;$('job-view').hidden=!job;
  if(!job){if(selectedJob&&boardJobs.length){/* Selected job left the board; keep its retained output visible. */}return;}
  const now=Number(observation?.observed_at)||Date.now()/1000,outcome=jobOutcome(job),runner=jobRunner(job);
  $('detail-state').dataset.state=job.state||'unknown';$('detail-state').textContent=stateLabel(job.state);$('detail-id').textContent=jobId(job);
  const flags=$('detail-flags');flags.replaceChildren(...jobFlags(job,now).map(flag=>{const node=element('div',`flag flag-${flag.kind}`);node.append(element('strong',null,flag.label),element('span',null,flag.detail));return node;}));
  const started=Number(job.started_at),finished=Number(job.finished_at),cpu=job.reserved_cpu??job.cpu,memory=job.reserved_memory_gb??job.memory_gb;
  const facts=[['Workload result',outcome.label,`fact-outcome outcome-${outcome.kind}`],['Submitted by',`${clientLabel(job.client)} (self-declared)`],['Runner',runner==='laptop'?'This laptop':'Spark'],['Runtime',job.runtime||'—'],['Reserved',cpu!=null?`${cpu} CPU · ${memory??'—'} GiB`:runner==='laptop'?'Not capped (native)':'—'],['Queued',relativeTime(job.created_at,now)],['Run time',Number.isFinite(started)&&started>0?duration((Number.isFinite(finished)&&finished>0?finished:now)-started)+(isActiveJob(job)?' so far':''):'Not started']];
  if(job.request_id)facts.push(['Request ID',String(job.request_id)]);
  if(job.cached_from)facts.push(['Cached from',String(job.cached_from).slice(0,16)]);
  renderFacts($('detail-facts'),facts);
  const cancellable=isActiveJob(job)&&!job.cancel_requested;$('cancel-job').hidden=!cancellable;$('cancel-job').querySelector('span').textContent=cancelArmed===jobId(job)?'Confirm cancel':'Cancel job';$('cancel-job').classList.toggle('armed',cancelArmed===jobId(job));
  renderLog();
  if(isActiveJob(job))startLogFollow();else stopLogFollow();
}
function renderLog(){if(!logState)return;$('job-output').textContent=logState.error||logState.text||(logState.loading?'Loading retained output…':'No output recorded yet.');$('log-meta').textContent=logState.offset?`${(logState.offset/1024).toFixed(1)} KiB retained${logTimer?' · following':''}`:logTimer?'Following':'';}
function decodeLog(result){if(typeof result.data==='string'){try{return new TextDecoder().decode(Uint8Array.from(atob(result.data),c=>c.charCodeAt(0)));}catch{}}return typeof result.text==='string'?result.text:'';}
async function loadLogs(reset=false){
  const state=logState;if(!state||state.loading)return;if(reset){state.text='';state.offset=0;}state.loading=true;state.error='';renderLog();
  try{const result=await command(['logs',state.job,'--since',String(state.offset)]);if(logState!==state)return;const chunk=decodeLog(result);state.text+=chunk;if(Number.isSafeInteger(result.next_offset))state.offset=result.next_offset;else state.offset+=new TextEncoder().encode(chunk).length;const pre=$('job-output'),atEnd=pre.scrollHeight-pre.scrollTop-pre.clientHeight<40;state.loading=false;renderLog();if(atEnd||reset)pre.scrollTop=pre.scrollHeight;}
  catch(error){if(logState===state){state.loading=false;state.error=failure(error);renderLog();}}
}
function startLogFollow(){if(!logTimer)logTimer=setInterval(()=>{if(!document.hidden&&currentView==='overview')loadLogs(false);},4000);}
function stopLogFollow(){clearInterval(logTimer);logTimer=null;}
$('refresh-output').addEventListener('click',()=>loadLogs(true));
$('copy-job-id').addEventListener('click',()=>copyText(selectedJob||'','Job ID'));
$('cancel-job').addEventListener('click',()=>busy($('cancel-job'),async()=>{const id=selectedJob;if(!id)return;if(cancelArmed!==id){cancelArmed=id;renderJobDetail();setTimeout(()=>{if(cancelArmed===id){cancelArmed=null;renderJobDetail();}},5000);return;}cancelArmed=null;await command(['cancel',id]);message('Cancellation requested. The runner reports the final state.');await refresh();}));

// Preferences, connection forms and pairing
function resourceTarget(config){return JSON.stringify(config?[config.ssh_alias||'',config.host_key_alias||'',config.user||'',config.port||22,config.hosts||[]]:null);}
async function loadPreferences(fillForm=true){const prefs=await invoke('preferences'),targetChanged=resourceTarget(savedConfig)!==resourceTarget(prefs.config);preferences=prefs;savedConfig=prefs.config||null;if(targetChanged){resourceGeneration++;resourceReady=false;resourceHardware=null;if(limits){$('resource-observation').textContent='Spark connection changed. Your draft is kept; read settings before saving to this Spark.';renderResourceDraft();}}laptop=prefs.laptop||laptop;const version=prefs.app_version||'0.3.0';$('app-version').textContent=version;$('installed-version').textContent=version;if(prefs.error)message(prefs.error.message||prefs.error);if(savedConfig){if(prefs.connection){savedConfig.connection_mode=prefs.connection.mode;savedConfig.connection_enabled=prefs.connection.enabled;}if(fillForm){const form=$('ssh-form');for(const [field,key] of [['host','ssh_alias'],['hostKeyAlias','host_key_alias'],['user','user'],['identity_file','identity_file'],['local_build_gate','local_build_gate']])form.elements[field].value=savedConfig[key]||'';form.elements.hostname.value=savedConfig.hosts?.[0]||'';form.elements.port.value=savedConfig.port||22;$('mode-form').elements.mode.value=configMode(savedConfig);$('bootstrap-remote').checked=savedConfig.allow_remote===true;}}else $('ssh-settings').open=true;renderLaptop();renderConnection();}
function renderLaptop(){if(Number.isSafeInteger(laptop?.concurrency_limit)){$('laptop-limit').value=laptop.concurrency_limit;$('laptop-limit').max=Math.min(64,laptop.logical_parallelism||64);$('laptop-limit').disabled=false;$('save-laptop').disabled=false;}}
$('connection-toggle').addEventListener('click',()=>busy($('connection-toggle'),async()=>{connecting=true;renderConnection();try{const response=await invoke('connection',{mode:null,enabled:savedConfig?.connection_enabled===false,laptopJobs:null});await loadPreferences(false);requireOk(response);await refresh();message(savedConfig?.connection_enabled===false?'Disconnected. Admitted jobs keep running.':'Saved connection checked.');}finally{connecting=false;renderConnection();}}));
$('mode-form').addEventListener('submit',event=>{event.preventDefault();busy(event.submitter,async()=>{requireOk(await invoke('connection',{mode:new FormData(event.target).get('mode'),enabled:null,laptopJobs:null}));observation=null;connecting=true;try{await loadPreferences(false);await refresh();}finally{connecting=false;renderConnection();}message('Connection mode saved. Your pairing is unchanged.');});});
$('ssh-form').addEventListener('submit',event=>{event.preventDefault();busy(event.submitter,async()=>{const form=new FormData(event.target),host=form.get('host').trim(),hostname=form.get('hostname').trim();const args=['setup','--ssh-alias',host,'--host-key-alias',form.get('hostKeyAlias').trim()||host,'--port',form.get('port')];if(hostname)args.push('--host',hostname);for(const fallback of (savedConfig?.hosts||[]).slice(1)){if(fallback!==hostname)args.push('--host',fallback);}for(const [field,flag] of [['user','--user'],['identity_file','--identity-file'],['local_build_gate','--local-build-gate']]){const value=form.get(field).trim();if(value)args.push(flag,value);}if(configMode(savedConfig)!=='lan-only')args.push('--allow-remote');await command(args);await loadPreferences();message('Connection saved. Choose Test connection when ready; Spark services were not restarted.');});});
$('test-connection').addEventListener('click',()=>busy($('test-connection'),async()=>{const result=await command(['doctor']);await refresh();message(result.checks?.docker&&result.scheduler?.healthy?'Connection verified. Docker and the shared scheduler are ready.':'Authenticated connection checked. Review the reported service health.');}));
$('bootstrap').addEventListener('click',()=>busy($('bootstrap'),async()=>{$('bootstrap-message').textContent='Installing the independent bridge over pinned SSH…';try{const result=requireOk(await invoke('bootstrap',{remoteAccess:$('bootstrap-remote').checked}));$('bootstrap-message').textContent=result.runtime_state==='building'?'Bridge installed. The pinned runtime is still preparing.':'Bridge installed. Its own services are ready to check.';await refresh();}catch(error){$('bootstrap-message').textContent=failure(error);throw error;}}));

// Resources
for(const [key,label,unit] of RESOURCE_FIELDS){const control=element('label',null,label);const input=document.createElement('input');input.name=key;input.type='number';input.min='1';input.step='1';input.required=true;input.setAttribute('aria-label',`${label} (${unit})`);input.setAttribute('aria-describedby','resource-validation');control.append(input,element('span','input-unit',unit));$('resource-fields').append(control);}
for(const preset of RESOURCE_PRESETS){const button=element('button','resource-preset');button.type='button';button.dataset.preset=preset.id;button.disabled=true;button.setAttribute('aria-pressed','false');button.append(element('strong',null,preset.name),element('span',null,preset.description),element('small',null,'Read Spark settings to choose'));button.addEventListener('click',()=>{fillResourceDraft(resourcePreset(preset.id,resourceHardware));renderResourceDraft();});$('resource-presets').append(button);}
function fillResourceDraft(policy){for(const [key] of RESOURCE_FIELDS)$('resources-form').elements[key].value=policy[key];}
function resourceDraft(){return Object.fromEntries(RESOURCE_FIELDS.map(([key])=>[key,Number($('resources-form').elements[key].value)]));}
function renderResourceDraft(){
  const draft=resourceDraft();let error='',policy=null;
  if(limits){try{policy=resourcePolicy(new Map(RESOURCE_FIELDS.map(([key])=>[key,$('resources-form').elements[key].value])),resourceHardware);}catch(reason){error=failure(reason);}}
  resourceDirty=!!limits&&!sameResourcePolicy(draft,limits);
  const configured=!resourceReady?'Unverified':resourceDirty?'Draft':'Saved';let presetName='Custom limits';
  for(const button of document.querySelectorAll('[data-preset]')){let preset=null;try{preset=resourcePreset(button.dataset.preset,resourceHardware);}catch{}const selected=policy&&sameResourcePolicy(policy,preset);button.disabled=!preset||resourceBusy;button.setAttribute('aria-pressed',String(!!selected));if(selected)presetName=RESOURCE_PRESETS.find(item=>item.id===button.dataset.preset).name;button.querySelector('small').textContent=preset?`${preset.cpu_per_job} cores · ${preset.memory_gb_per_job} GiB per job`:'Read Spark settings to choose';}
  $('resource-fields').disabled=!limits||resourceBusy;
  const hardware=resourceHardware,cores=hardware?.cpu_count,memory=hardware?.memory_gb;
  $('resource-hardware').textContent=hardware?`Observed Spark: ${cores} CPU cores · ${memory} GiB memory`:'Hardware bounds were not reported.';
  const percent=(value,total)=>Number.isFinite(value)&&total>0?Math.min(100,Math.max(0,value/total*100)):0;
  const cpuBudget=Number.isFinite(draft.cpu_total)?draft.cpu_total:0,memoryBudget=Number.isFinite(draft.memory_gb_total)?draft.memory_gb_total:0,headroom=Number.isFinite(draft.headroom_gb)?draft.headroom_gb:0;
  $('cpu-budget-label').textContent=limits&&cores?`${cpuBudget} / ${cores} cores`:'Not observed';
  $('memory-budget-label').textContent=limits&&memory?`${memoryBudget} / ${memory} GiB`:'Not observed';
  $('cpu-budget-fill').style.width=`${percent(cpuBudget,cores)}%`;
  $('memory-budget-fill').style.width=`${percent(memoryBudget,memory)}%`;
  $('memory-headroom-fill').style.width=`${Math.min(percent(headroom,memory),100-percent(memoryBudget,memory))}%`;
  $('cpu-budget-note').textContent=limits&&cores?`${configured} build budget · ${Math.max(0,cores-cpuBudget)} cores outside this budget.`:'Read your Spark’s hardware to compare its capacity.';
  $('memory-budget-note').textContent=limits&&memory?`${configured} build budget · ${headroom} GiB minimum headroom.`:'Headroom stays outside the build budget.';
  $('cpu-budget-bar').setAttribute('aria-label',limits&&cores?`${configured} CPU budget ${cpuBudget} of ${cores} hardware cores. Not live usage.`:'CPU budget not observed');
  $('memory-budget-bar').setAttribute('aria-label',limits&&memory?`${configured} memory budget ${memoryBudget} of ${memory} GiB hardware memory; ${headroom} GiB minimum headroom. Not live usage.`:'Memory budget not observed');
  $('resource-draft-summary').textContent=policy?`${configured}: ${presetName} · up to ${policy.cpu_per_job} cores and ${policy.memory_gb_per_job} GiB per job · ${policy.client_jobs} ${policy.client_jobs===1?'job':'jobs'} per agent.`:limits?'Adjust the limits below to create a valid budget.':'Your saved limits will appear here.';
  $('resource-validation').textContent=error||(!limits?'Read the Spark policy before editing.':!resourceReady?'Current Spark settings need a fresh read. Your draft is kept; read settings again before saving.':resourceDirty?'Unsaved changes. CPU per job ≤ per agent ≤ total; memory budget + headroom fits your Spark.':'Saved limits loaded. Choose a preset or open advanced limits to make changes.');
  $('resource-validation').dataset.state=error?'error':resourceDirty?'draft':'saved';
  $('save-resources').disabled=!policy||!resourceDirty||!resourceReady||resourceBusy;
  $('discard-resources').hidden=!resourceDirty;$('discard-resources').disabled=resourceBusy;
}
async function readResources(){
  if(resourceBusy)return;const generation=resourceGeneration;resourceBusy=true;$('refresh-resources').disabled=true;renderResourceDraft();
  try{const result=requireOk(await invoke('resources',{policy:null}));if(generation!==resourceGeneration)throw new Error('Spark connection changed during the request. Read settings again before saving.');const keepDraft=resourceDirty;limits=result.limits;resourceHardware=result.hardware||resourceHardware;resourceReady=true;if(!keepDraft)fillResourceDraft(limits);$('resource-observation').textContent=keepDraft?'Latest saved policy refreshed. Your unsaved changes are kept.':'Current Spark policy loaded.';renderAgents();}
  catch(error){resourceReady=false;$('resource-observation').textContent=`${failure(error)}${limits?' Your last observed policy and draft are retained.':''}`;}
  finally{resourceBusy=false;$('refresh-resources').disabled=false;renderResourceDraft();}
}
$('refresh-resources').addEventListener('click',readResources);
$('resource-fields').addEventListener('input',renderResourceDraft);
$('discard-resources').addEventListener('click',()=>{if(limits){fillResourceDraft(limits);renderResourceDraft();}});
$('resources-form').addEventListener('submit',async event=>{
  event.preventDefault();if(resourceBusy||!resourceReady||$('save-resources').disabled)return;const generation=resourceGeneration;
  try{const policy=resourcePolicy(new FormData(event.target),resourceHardware);resourceBusy=true;$('refresh-resources').disabled=true;renderResourceDraft();const result=requireOk(await invoke('resources',{policy}));if(generation!==resourceGeneration)throw new Error('Spark connection changed during the request. Read settings again before saving.');limits=result.limits;resourceHardware=result.hardware||resourceHardware;resourceReady=true;fillResourceDraft(limits);$('resource-observation').textContent='Spark limits saved and read back. Running jobs retain their admitted limits.';renderAgents();}
  catch(error){$('resource-observation').textContent=`${failure(error)} Your unsaved changes are kept.`;}
  finally{resourceBusy=false;$('refresh-resources').disabled=false;renderResourceDraft();}
});
$('laptop-form').addEventListener('submit',event=>{event.preventDefault();busy(event.submitter,async()=>{const value=Number(new FormData(event.target).get('laptop_jobs'));if(!Number.isSafeInteger(value)||value<1)throw new Error('Choose a positive whole number of laptop jobs.');const result=requireOk(await invoke('connection',{mode:null,enabled:null,laptopJobs:value}));laptop=result.laptop;renderLaptop();message('Laptop concurrency saved. Native CPU and memory limits are unchanged.');await refresh();});});

// Pairing
async function readPairState(){if(pairTimer)return;try{const result=requireOk(await invoke('link',{op:'status'}));$('pair-art').hidden=result.linked===true;$('start-pair').textContent=result.linked?'Pair this laptop again':'Create pairing code';$('pair-saved').hidden=!result.linked;$('pair-saved').textContent=result.linked?(result.reachable===true?'This laptop is paired. The paired route is reachable.':result.reachable===false?'This laptop is paired. The paired route is not reachable right now.':'This laptop is paired. Its saved identity is preserved.'):'';}catch(error){$('pair-saved').hidden=false;$('pair-saved').textContent=failure(error);}}
function svgFor(art){const n=art.size,c=art.logo.x+art.logo.size/2;return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${n} ${n}"><defs><linearGradient id="ink" x2="1" y2="1"><stop stop-color="#1e3a8a"/><stop offset=".5" stop-color="#4c1d95"/><stop offset="1" stop-color="#134e4a"/></linearGradient></defs><rect width="${n}" height="${n}" fill="white"/><g fill="url(#ink)">${art.dots.map(p=>`<circle cx="${p.x}" cy="${p.y}" r=".45"/>`).join('')}${art.eyes.map(e=>`<path d="${eyeRing(e.x,e.y)}" fill-rule="evenodd"/><path d="${eyePupil(e.x,e.y)}"/>`).join('')}${art.aligns.map(e=>`<path d="${alignRing(e.x,e.y)}" fill-rule="evenodd"/><path d="${alignDot(e.x,e.y)}"/>`).join('')}<path d="${sparkPath(c,c,art.logo.size*.42)}"/></g></svg>`;}
function animate(sim,done){cancelAnimationFrame(pairFrame);let previous=0;const tick=at=>{step(sim,previous?at-previous:16);previous=at;const ctx=pairCanvas.getContext('2d');draw(ctx,sim,{scale:252/sim.size,offset:14,dpr:2,width:280,height:280});if(isActive(sim))pairFrame=requestAnimationFrame(tick);else{pairCanvas.hidden=true;done?.();}};pairFrame=requestAnimationFrame(tick);}
function showQr(result){pairArt=qrArt(result.qr_url);const container=$('pair-art');container.replaceChildren();const tile=element('div','qr-tile');if(pairArt){tile.innerHTML=svgFor(pairArt);}else{const image=document.createElement('img');image.alt='Scan this code to approve your laptop';image.src='data:image/svg+xml;base64,'+btoa(unescape(encodeURIComponent(result.qr_svg||'')));image.width=252;tile.append(image);}container.append(tile,element('div','qr-halo'));if(pairArt&&!reduced){tile.hidden=true;pairCanvas=document.createElement('canvas');pairCanvas.width=560;pairCanvas.height=560;container.append(pairCanvas);pairSim=createSim({size:pairArt.size,count:pairArt.cells.length});setTargets(pairSim,pairArt.cells);animate(pairSim,()=>{tile.hidden=false;});}$('approval-code').hidden=false;$('approval-code').textContent=result.verification_code||'';}
function pairedMark(container){container.replaceChildren(element('div','paired-mark','✓'));}
function pairedAnimation(){const container=$('pair-art');container.querySelector('.qr-tile')?.remove();container.querySelector('.qr-halo')?.remove();if(pairArt&&!reduced&&pairCanvas){pairCanvas.hidden=false;placeOnCells(pairSim,pairArt.cells);burst(pairSim,checkPoints(pairArt.size));animate(pairSim,()=>pairedMark(container));}else pairedMark(container);}
async function pollPair(){try{const result=await invoke('link',{op:'pair-status'});const state=result.state||'failed';if(state==='pending'&&lastPairState!==state)showQr(result);if(state==='paired'&&lastPairState!==state){pairedAnimation();$('pair-message').textContent='This laptop is paired. The link identity is saved securely.';$('approval-code').textContent='';$('approval-code').hidden=true;stopPairPoll();refresh();}else if(state==='pending'){$('pair-message').textContent=`Scan with your phone and enter these six digits. Expires in ${Math.max(0,Math.ceil(result.expires_at-Date.now()/1000))} seconds.`;}else if(['failed','cancelled','expired'].includes(state)||result.ok===false){stopPairPoll();$('pair-art').replaceChildren();$('approval-code').textContent='';$('approval-code').hidden=true;$('pair-message').textContent=result.error?.message||`Pairing ${state}. Create a new code to try again.`;}lastPairState=state;}catch(error){stopPairPoll();$('pair-message').textContent=String(error.message||error);}}
function stopPairPoll(){clearInterval(pairTimer);pairTimer=null;$('start-pair').disabled=false;$('cancel-pair').hidden=true;}
$('start-pair').addEventListener('click',async()=>{$('pair-art').hidden=false;$('start-pair').disabled=true;$('cancel-pair').hidden=false;$('pair-message').textContent='Creating a private pairing ticket over SSH…';lastPairState='';try{requireOk(await invoke('link',{op:'pair-start'}));pairTimer=setInterval(pollPair,1000);await pollPair();}catch(error){stopPairPoll();$('pair-message').textContent=String(error.message||error);}});
$('cancel-pair').addEventListener('click',async()=>{await invoke('link',{op:'pair-cancel'});await pollPair();});

// Agents
function renderAgents(){const state=connectionView(observation,savedConfig,connecting),paths=agentPaths(agent,preferences);$('agent-file').textContent=agent==='codex'?'Add to ~/.codex/config.toml':'Add to your project’s .mcp.json';$('agent-label').textContent=agent;$('agent-config').textContent=agentConfiguration(agent,paths.path,paths.stateRoot);$('agent-prompt').textContent=agentPrompt({agent,...paths,config:savedConfig,connection:state,limits:resourceReady?limits:null});document.querySelectorAll('[data-agent]').forEach(el=>{el.classList.toggle('selected',el.dataset.agent===agent);el.setAttribute('aria-pressed',String(el.dataset.agent===agent));});}
document.querySelectorAll('[data-agent]').forEach(el=>el.addEventListener('click',()=>{agent=el.dataset.agent;renderAgents();}));
async function copyText(text,label){try{await navigator.clipboard.writeText(text);message(`${label} copied.`);}catch{message(`Select and copy the ${label.toLowerCase()} manually.`);}}
$('copy-config').addEventListener('click',()=>copyText($('agent-config').textContent,'Agent configuration'));$('copy-prompt').addEventListener('click',()=>copyText($('agent-prompt').textContent,'Agent prompt'));

// Updates
const updateLabels={idle:'Not checked',checking:'Checking for updates…',current:'Up to date',available:'Update available',awaiting_approval:'Waiting for your approval',approved:'Update approved',downloading:'Downloading update…',ready:'Ready to install',installing:'Starting the installer…',denied:'Approval declined',expired:'Approval expired',error:'Update needs attention'};
function renderUpdate(){const status=update.status||'idle';$('update-state').textContent=updateLabels[status]||'Update status';$('update-state').dataset.state=status;$('update-title').textContent=status==='idle'?'Check for an update':updateLabels[status]||'Update status';$('update-notice-label').textContent=['available','approved','ready'].includes(status)?'Update':'Updates';$('update-notice-dot').hidden=!['available','approved','ready'].includes(status);$('update-message').textContent=update.message||'Only verified Marketplace releases are eligible.';if(update.current_version)$('installed-version').textContent=update.current_version;$('available-version').hidden=!update.available_version;$('available-version').textContent=update.available_version?`Available version: ${update.available_version}`:'';const last=update.last_update;$('last-update').hidden=!last;$('last-update').textContent=last?'Previous update: '+(last.message||last.status||'A result was recorded.'):'';const awaiting=status==='awaiting_approval';$('copy-update-link').hidden=!(awaiting&&/^https:\/\/deloresai\.com\/native-updates\/[A-Za-z0-9_-]{24}$/.test(update.verification_uri||''));$('update-approval').hidden=!awaiting;$('update-code').textContent=awaiting?update.verification_code||'':'';$('update-expiry').textContent=awaiting&&update.expires_at?`Approval expires: ${update.expires_at}`:'';const downloading=status==='downloading';$('update-progress').hidden=!downloading;$('update-bytes').hidden=!downloading;if(Number.isFinite(update.total_bytes)&&update.total_bytes>0){$('update-progress').max=update.total_bytes;$('update-progress').value=Math.min(update.progress_bytes||0,update.total_bytes);$('update-bytes').textContent=`${((update.progress_bytes||0)/1048576).toFixed(1)} / ${(update.total_bytes/1048576).toFixed(1)} MiB`;}else{$('update-progress').removeAttribute('value');$('update-bytes').textContent='Receiving the verified installer…';}for(const button of document.querySelectorAll('[data-update-action]')){const action=button.dataset.updateAction;button.hidden=action==='authorize'?!['available','denied','expired'].includes(status):action==='download'?status!=='approved':action==='install'?status!=='ready':false;button.disabled=updateBusy||update.busy===true||action==='check'&&['checking','awaiting_approval','downloading','installing'].includes(status);}}
function scheduleUpdatePoll(force=false){clearTimeout(updateTimer);if(force||update.busy===true||['awaiting_approval','checking','downloading','installing'].includes(update.status))updateTimer=setTimeout(()=>performUpdate(update.status==='awaiting_approval'&&update.busy!==true?'poll':'status',true),6000);}
async function performUpdate(action,background=false){if(updateBusy)return;updateBusy=true;renderUpdate();try{const result=await invoke('update_action',{action});if(result&&typeof result==='object')update=result;else throw new Error('No structured update status was returned.');if(result.ok===false&&!result.status)update={...result,status:'error',message:result.error?.message||result.message};}catch(error){update={...update,status:'error',busy:false,message:failure(error)};}finally{updateBusy=false;renderUpdate();scheduleUpdatePoll(!['status','poll'].includes(action));}}
document.querySelectorAll('[data-update-action]').forEach(button=>button.addEventListener('click',()=>performUpdate(button.dataset.updateAction)));
$('copy-update-link').addEventListener('click',async()=>{const uri=update.verification_uri;if(!/^https:\/\/deloresai\.com\/native-updates\/[A-Za-z0-9_-]{24}$/.test(uri||''))return;try{await navigator.clipboard.writeText(uri);$('copy-update-link').textContent='Link copied';}catch{$('update-message').textContent='Approval page: '+uri;}});
$('open-updates').addEventListener('click',()=>{$('updates-dialog').showModal();performUpdate('status');});$('close-updates').addEventListener('click',()=>$('updates-dialog').close());

motionQuery.addEventListener('change',event=>{reduced=event.matches;renderConnection();if(reduced)cancelAnimationFrame(pairFrame);});
async function init(){try{await loadPreferences();}catch(error){message(failure(error));}await refresh();performUpdate('status',true);}
init();const refreshTimer=setInterval(()=>{if(!document.hidden&&['overview','connection'].includes(currentView))refresh();},15000);
window.addEventListener('beforeunload',()=>{clearInterval(refreshTimer);clearInterval(pairTimer);stopLogFollow();clearTimeout(updateTimer);cancelAnimationFrame(pairFrame);visuals.forEach(visual=>visual.dispose());});

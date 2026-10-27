import {qrArt,eyeRing,eyePupil,alignRing,alignDot,sparkPath,checkPoints} from './vendor/sparkLinkQr.mjs';
import {createSim,setTargets,step,draw,isActive,placeOnCells,burst} from './vendor/sparkParticles.mjs';
import {MODE_LABELS,RESOURCE_FIELDS,RESOURCE_PRESETS,configMode,connectionView,jobsFrom,reconcileObservedJobs,jobPage,resourcePolicy,resourcePreset,sameResourcePolicy,agentPaths,agentConfiguration,agentPrompt,
  isActiveJob,jobId,jobRunner,jobRunnerLabel,jobToolsLabel,namedComputers,deviceRows,fleetSummary,workSummary,JOB_KINDS,deviceSlug,deviceSetupCommand,probeSummary,jobOutcome,jobFlags,filterJobs,clientLabel,duration,relativeTime,boardSummary,routeAddress,routeAddressLabel,
  FIND_INTRO,findSparkControl,discoveryView,discoverySaved,otherLanAddresses,sparkLink,plainError,serviceView,INTERNET_COPY,internetView,downloadsView,jobInternet,formatGiB,laptopLink} from './model.mjs';
import {createFleetMap} from './fleet-map.mjs';
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
let findBusy=false,findMessage=null;
let refreshPromise=null,refreshAgain=false,lastGoodObservation=null,lastFailure=null,health={failures:0,lastGoodAt:0};
let internet=null,downloads=null,extrasEpoch=0,downloadsBusy=false,clearArmed=false,downloadsDirty=false,internetBusy=false,internetMessage='',downloadsMessage='';
let computers=[],computersBusy=false,computersLoaded=false,computersMessage='',computerObservation=null,computerObservationStale=false,jobDevice='',selectedDevice='spark';
const logCache=new Map(),jobCache=new Map();
let fleetMap=null;
function element(tag,className,text){const node=document.createElement(tag);if(className)node.className=className;if(text!=null)node.textContent=text;return node;}
function message(text){clearTimeout(messageTimer);$('message').textContent=text||'';$('message').hidden=!text;if(text)messageTimer=setTimeout(()=>{$('message').hidden=true;},7000);}
function requireOk(value){if(value?.ok===false){const error=new Error(value.error?.message||value.message||'The operation did not complete.');error.code=value.error?.code??value.code;error.kind=value.error?.kind??value.kind;throw error;}return value;}
async function command(args){return requireOk(await invoke('cli',{args}));}
const failure=plainError;
const connectionState=()=>connectionView(lastFailure||observation,savedConfig,connecting,health);
async function busy(button,action){if(button.disabled)return;button.disabled=true;try{await action();}catch(error){message(failure(error));}finally{button.disabled=false;}}

// Theme follows CodaClauda: dark by default, remembered per viewer when storage is available.
function applyTheme(theme){document.documentElement.dataset.theme=theme;const light=theme==='light';$('theme-toggle').setAttribute('aria-label',light?'Switch to dark theme':'Switch to light theme');$('theme-toggle').querySelector('use').setAttribute('href',light?'#i-moon':'#i-sun');}
try{applyTheme(localStorage.getItem('dgxbridge-theme')==='light'?'light':'dark');}catch{applyTheme('dark');}
$('theme-toggle').addEventListener('click',()=>{const next=document.documentElement.dataset.theme==='light'?'dark':'light';applyTheme(next);try{localStorage.setItem('dgxbridge-theme',next);}catch{}if(fleetMap)renderDevices();});

const TITLES={overview:'Jobs',devices:'Devices',resources:'Job settings',agents:'Agent setup'};
function view(name){currentView=name;document.querySelectorAll('.view').forEach(el=>el.hidden=el.id!==name);document.querySelectorAll('.nav[data-view]').forEach(el=>{const active=el.dataset.view===name;el.classList.toggle('active',active);if(active)el.setAttribute('aria-current','page');else el.removeAttribute('aria-current');});$('page-title').textContent=TITLES[name];document.querySelector('.views').scrollTop=0;if(name==='resources'&&!limits&&!resourceReady)readResources();if(name!=='devices')expandMap(false);if(name==='devices'){renderDevices();if(selectedDevice==='spark')readPairState();if(!computersLoaded)readComputers();}fleetMap?.setActive(name==='devices');}
document.querySelectorAll('[data-view]').forEach(button=>button.addEventListener('click',()=>{if(button.dataset.agentChoice){agent=button.dataset.agentChoice;renderAgents();}if(button.dataset.device)selectedDevice=button.dataset.device;view(button.dataset.view);}));

// Connection
const STATE_LABELS={connected:'Spark connected',disconnected:'Disconnected',connecting:'Connecting…',reconnecting:'Reconnecting…',error:'Needs attention',unavailable:'Spark unavailable'};
function renderConnection(){
  const state=connectionState(),spark=observation?.devices?.find(device=>device.id==='spark'),used=routeAddress(observation,savedConfig);
  const unconfigured=!savedConfig&&state.state==='error',shown=unconfigured?'unavailable':state.state,label=unconfigured?'Spark not set up':STATE_LABELS[state.state];
  document.querySelectorAll('[data-connection-state]').forEach(el=>el.textContent=label);
  document.querySelectorAll('[data-connection-dot]').forEach(el=>el.dataset.state=shown);
  document.querySelectorAll('[data-connection-link]').forEach(el=>el.dataset.state=shown);
  document.querySelectorAll('[data-mode-label]').forEach(el=>el.textContent=MODE_LABELS[state.mode]);
  document.querySelectorAll('[data-spark-name]').forEach(el=>el.textContent=savedConfig?.ssh_alias||'Your Spark');
  const routeText=state.connected?state.route:unconfigured?'Set up your Spark to start':label;
  document.querySelectorAll('[data-connection-route]').forEach(el=>el.textContent=routeText);
  const note=!state.enabled?'New requests are paused. Running jobs and the saved pairing are unchanged.':state.connected?(state.schedulerHealthy?'Your Spark is connected and ready for jobs.':'Connected, but the Spark job service is not responding. New jobs may have to wait.'):(state.reason||'Set up your Spark, then connect.');
  const fullNote=health.failures&&lastGoodObservation?`${note} Last successful check: ${relativeTime(health.lastGoodAt)}.`:note;
  document.querySelectorAll('[data-connection-note]').forEach(el=>el.textContent=fullNote);
  $('connect-spark').hidden=!!savedConfig;$('connection-toggle').hidden=!savedConfig;$('connection-toggle').textContent=state.enabled?'Disconnect':'Connect';$('connection-toggle').className=`button ${state.enabled?'secondary':'primary'}`;$('connection-toggle').disabled=connecting||!savedConfig;
  const heartbeat=Number(spark?.scheduler?.last_heartbeat),hardware=spark?.hardware;
  const link=sparkLink(observation);
  const laptopNetwork=laptopLink(observation);
  const facts=[['Connected through',state.route],['Spark network',state.connected&&link?link.label:state.connected?'Not reported':'Not connected'],['Laptop network',laptopNetwork.label],['Job service',state.connected&&spark?.scheduler?(spark.scheduler.healthy?'Running':'Not responding'):'Not checked'],['Spark hardware',hardware?.cpu_count?`${hardware.cpu_count} CPU cores · ${hardware.memory_gb} GiB`:'Not observed'],['Last successful check',health.lastGoodAt?relativeTime(health.lastGoodAt):'Not checked']];
  renderFacts($('route-facts'),facts);$('laptop-network-note').textContent=laptopNetwork.note;$('laptop-network-note').classList.toggle('link-warning',laptopNetwork.slow===true);
  renderFacts($('route-details'),[['Address used',routeAddressLabel(used)],['Last heard from',Number.isFinite(heartbeat)&&heartbeat>0?relativeTime(heartbeat,observation?.observed_at):'Not reported'],...state.details]);
  $('spark-network-warn').hidden=!state.connected||!link?.warning;$('spark-network-text').textContent=link?.warning||'';$('check-cable').hidden=link?.action!=='find';
  $('network-note').hidden=state.connected||!state.action;$('network-note-text').textContent=state.action==='find'?"Can't reach your Spark here. Find it on this network?":state.reason;
  $('connection-help').textContent={find:'Find my Spark',repair:'Open repair',setup:'Connect your Spark',pair:'Pair this laptop',connect:'Connect'}[state.action]||'Check connection';
  const iv=internetView(internet),dv=downloadsView(downloads);$('spark-extras').replaceChildren(element('span',null,`Job internet: ${iv.label}`),element('span',null,`Saved downloads: ${dv.label}`));
  renderAgents();renderFindControls();
}
function renderFacts(list,facts){list.replaceChildren(...facts.flatMap(([term,value,className])=>{const wrap=element('div',className||'');wrap.append(element('dt',null,term),element('dd',null,value));return [wrap];}));}
function acceptExtras(result){const spark=result?.devices?.find(d=>d.id==='spark');if(spark){internet=spark.internet??null;downloads=spark.downloads??null;}else{if(Object.hasOwn(result||{},'internet'))internet=result.internet;if(Object.hasOwn(result||{},'downloads'))downloads=result.downloads;}renderExtras();}
async function refresh(){
  if(refreshPromise){refreshAgain=true;return refreshPromise;}
  refreshPromise=(async()=>{do{
    refreshAgain=false;const generation=resourceGeneration,epoch=extrasEpoch;refreshing=true;$('refresh').disabled=true;$('refresh').classList.add('spinning');
    try{
      const result=await command(['devices']);if(generation!==resourceGeneration){refreshAgain=true;continue;}computerObservation=result;computerObservationStale=false;
      const spark=result?.devices?.find(d=>d.id==='spark'),good=spark?.reachable===true&&spark.transport?.authenticated===true,incomingJobs=reconcileObservedJobs(boardJobs,result);
      if(!good&&savedConfig?.connection_enabled!==false&&lastGoodObservation){lastFailure=result;health.failures++;observation=lastGoodObservation;}
      else{observation=result;lastFailure=null;boardJobs=incomingJobs;for(const job of boardJobs)jobCache.set(jobId(job),job);if(good){lastGoodObservation=result;health={failures:0,lastGoodAt:Date.now()/1000};}else health={failures:0,lastGoodAt:0};}
      if(!good&&lastGoodObservation&&namedComputers(result,computers).length){boardJobs=jobsFrom({jobs:[...boardJobs.filter(job=>jobRunner(job)==='spark'),...incomingJobs]});for(const job of boardJobs)jobCache.set(jobId(job),job);}
      if(epoch===extrasEpoch&&!resourceBusy&&!downloadsBusy&&good)acceptExtras(result);
      $('connection-detail').textContent=JSON.stringify(result,null,2);
    }catch(error){if(generation!==resourceGeneration){refreshAgain=true;continue;}computerObservationStale=true;health.failures++;lastFailure={ok:false,error:{message:error.message||String(error),code:error.code,kind:error.kind}};observation=lastGoodObservation||lastFailure;$('connection-detail').textContent=error.message||String(error);}
    finally{renderConnection();renderBoard();renderExtras();renderDevices();}
  }while(refreshAgain);})().finally(()=>{refreshPromise=null;refreshing=false;$('refresh').disabled=false;$('refresh').classList.remove('spinning');});
  return refreshPromise;
}
$('connection-help').addEventListener('click',()=>{const action=connectionState().action;if(action==='find')$('find-spark').click();else if(action==='connect')$('connection-toggle').click();else if(action==='setup')openWizard('spark');else if(action==='repair')$('bootstrap').scrollIntoView({block:'center'});else if(action==='pair')$('start-pair').scrollIntoView({block:'center'});});
$('check-cable').addEventListener('click',()=>busy($('check-cable'),async()=>{await refresh();await refresh();message(connectionState().connected?'Connection choices checked. The displayed route is the one that answered.':"The Spark didn't confirm a connection. Check the status above before trying again.");}));
$('refresh').addEventListener('click',refresh);

// Devices: this laptop, the Spark and every added computer, shown the same way.
const DEVICE_ICONS={laptop:'#i-laptop',spark:'#i-spark',computer:'#i-desktop'};
function currentDevices(){return deviceRows({observation:computerObservation||observation,nodes:computers,jobs:boardJobs,connection:connectionState(),config:savedConfig,stale:computerObservationStale});}
function deviceIcon(kind){const icon=element('span','device-icon');icon.innerHTML=`<svg class="icon"><use href="${DEVICE_ICONS[kind]}"/></svg>`;return icon;}
function deviceCard(row,mode){
  const compact=mode!=='grid',card=element('button',`device-card${compact?' compact':''}${mode==='map'?' map-label':''}`);card.type='button';card.setAttribute('role','listitem');card.dataset.device=row.id;card.dataset.state=row.status.kind;
  if(row.kind==='computer')card.dataset.node=row.id;
  if(mode==='strip'&&['laptop','spark'].includes(row.id))card.id=`${row.id}-card`;
  if(mode!=='strip'&&row.id===selectedDevice)card.setAttribute('aria-current','true');
  const head=element('span','device-head'),title=element('span','device-title'),status=element('span','device-status computer-status',row.status.label);
  title.append(element('strong',null,row.name),element('small',null,row.detail));status.dataset.state=row.status.kind;head.append(deviceIcon(row.kind),title,status);card.append(head);
  if(!compact)card.append(element('span','device-good',row.goodFor));
  const specs=element('span','device-specs');for(const spec of row.specs.slice(0,compact?2:4))specs.append(element('span','device-spec',spec));card.append(specs);
  if(row.link&&!row.specs.slice(0,compact?2:4).includes(row.link.label)){const link=element('span','device-link',row.link.label);link.dataset.measured=String(row.link.measured);link.title=row.link.measured?`Measured on the last large transfer${row.link.measuredAt?` (${relativeTime(row.link.measuredAt)})`:''}`:'Reported link rate; real speed is measured on the first large transfer';card.append(link);}
  if(row.kind==='laptop'&&mode!=='strip'){const who=Object.entries(row.agents||{}).map(([name,count])=>`${name} ${count}`).join(' · ');card.append(element('span','device-link',who?`Agents: ${who}`:'No agent jobs running'));}
  if(row.error&&mode!=='strip'){const error=element('span','fine-print computer-error',row.error);error.title=row.error;card.append(error);}
  card.setAttribute('aria-label',`${row.name}: ${row.status.label}. ${row.goodFor}.`);
  card.addEventListener('click',()=>{selectedDevice=row.id;if(mode==='strip')view('devices');else{renderDevices();if(row.id==='spark')readPairState();}});
  return card;
}
function renderFleet(rows=currentDevices()){const summary=fleetSummary(rows);$('fleet-label').textContent=summary.label;document.querySelectorAll('[data-fleet-dot]').forEach(el=>el.dataset.state=summary.kind==='ready'?'connected':summary.kind==='setup'?'unavailable':'error');}
function renderDeviceStrip(){$('device-strip').replaceChildren(...currentDevices().map(row=>deviceCard(row,'strip')));}
function mapData(rows){
  const hub=rows.find(row=>row.kind==='laptop');
  return {hub:{id:'laptop',work:hub?.work||{}},kinds:JOB_KINDS,devices:rows.filter(row=>row.kind!=='laptop').map(row=>({id:row.id,status:row.status.kind,gpu:row.gpu,memory:row.memory,mbps:row.link?.mbps??null,work:row.work}))};
}
function renderLegend(rows){
  const legend=$('fleet-legend'),work=workSummary(rows);
  legend.replaceChildren(...(work.length?work.map(item=>{const chip=element('span','legend-item',`${item.label} ${item.count}`);chip.style.setProperty('--kind',item.color);return chip;}):[element('span','legend-note','No jobs running. Each link streams at its measured speed, or its link rate until a large transfer is measured.')]));
}
// The map can fill the window; Esc or the same button puts it back.
function expandMap(on){const map=$('fleet-map');map.classList.toggle('expanded',on);$('map-expand').setAttribute('aria-pressed',String(on));$('map-expand').querySelector('span').textContent=on?'Close':'Expand';document.body.classList.toggle('map-open',on);}
$('map-expand').addEventListener('click',()=>expandMap(!$('fleet-map').classList.contains('expanded')));
$('map-reset').addEventListener('click',()=>fleetMap?.resetView());
document.addEventListener('keydown',event=>{if(event.key==='Escape'&&$('fleet-map').classList.contains('expanded')){expandMap(false);event.stopPropagation();}});
function renderDevices(){
  const rows=currentDevices();renderFleet(rows);
  if(!rows.some(row=>row.id===selectedDevice))selectedDevice='spark';
  if(!fleetMap)fleetMap=createFleetMap($('fleet-map'),{labelFor:id=>$('device-grid').querySelector(`[data-device="${id}"]`),onSelect:id=>{selectedDevice=id;renderDevices();if(id==='spark')readPairState();}});
  $('fleet-map').querySelector('.map-tools').hidden=!fleetMap.available||rows.length-1>6;
  const mode=fleetMap.available&&rows.length-1<=6?'map':'grid';
  $('device-grid').replaceChildren(...rows.map(row=>deviceCard(row,mode)));renderLegend(rows);
  if(fleetMap.available)fleetMap.render(mapData(rows),{reduced,theme:document.documentElement.dataset.theme});
  fleetMap.setActive(currentView==='devices');
  const stale=computerObservationStale?'Device availability could not be refreshed. Showing what was last seen; jobs are kept.':'';
  $('computers-status').textContent=computersBusy?(computersMessage||'Checking devices…'):[computersMessage,stale].filter(Boolean).join(' ');
  $('refresh-computers').disabled=computersBusy;$('add-device').disabled=computersBusy;
  const row=rows.find(item=>item.id===selectedDevice);
  $('spark-detail').hidden=selectedDevice!=='spark';$('laptop-detail').hidden=selectedDevice!=='laptop';$('node-detail').hidden=row?.kind!=='computer';
  if(row?.kind==='computer')renderNodeDetail(row);else if(row?.kind==='laptop')renderLaptopDetail(row);
}
function panelHead(row){const head=element('div','card-top'),title=element('div','title-row');const text=element('div');text.append(element('h2',null,row.name),element('p','card-sub',`${row.detail} · ${row.status.label}`));title.append(deviceIcon(row.kind),text);head.append(title);return head;}
function renderNodeDetail(row){
  const card=element('article','card device-detail-card');card.append(panelHead(row),element('p','device-good',row.goodFor));
  const facts=element('dl','facts');renderFacts(facts,[...(row.facts||[]),['Agents choose it with',`--device ${row.id}`]]);card.append(facts);
  if(row.error)card.append(element('p','fine-print computer-error',row.error));
  const actions=element('div','device-actions'),button=(label,key,handler,disabled)=>{const b=element('button','button secondary small',label);b.type='button';b.dataset[key]=row.id;b.disabled=computersBusy||disabled;b.addEventListener('click',handler);actions.append(b);return b;};
  button(row.enabled?'Pause new jobs':'Resume new jobs','nodeToggle',()=>changeComputer(['node','enable',row.id,row.enabled?'off':'on'],row.enabled?`${row.name} is paused. Jobs already running there finish normally.`:`${row.name} takes new jobs again.`),false);
  button('Set up GPU tools','nodeGpu',()=>installComputer(row.id,row.name,true),!row.enabled);
  button('Reinstall bridge','nodeInstall',()=>installComputer(row.id,row.name,false),!row.enabled);
  card.append(actions,element('p','fine-print','Pausing stops new jobs here. Reinstalling repairs RTXBridge on the device and keeps its jobs and settings.'));
  $('node-detail').replaceChildren(card);
}
function renderLaptopDetail(row){
  const card=element('article','card device-detail-card'),link=laptopLink(computerObservation||observation);card.append(panelHead(row),element('p','device-good',row.goodFor));
  const facts=element('dl','facts');renderFacts(facts,[['Job slots',Number.isSafeInteger(laptop?.concurrency_limit)?`${laptop.concurrency_limit} at a time`:'Not reported'],['CPU',row.specs.find(spec=>spec.includes('CPU'))||'Not reported'],['Network',link.label],['Limits','Jobs run as you, without CPU or memory caps']]);card.append(facts);
  if(link.note)card.append(element('p',`fine-print${link.slow?' link-warning':''}`,link.note));
  const actions=element('div','device-actions'),slots=element('button','button secondary small','Change job slots');slots.type='button';slots.addEventListener('click',()=>view('resources'));actions.append(slots);card.append(actions);
  $('laptop-detail').replaceChildren(card);
}
async function readComputers(){
  if(computersBusy)return;computersBusy=true;renderDevices();
  try{const result=await command(['node','list']);computers=Array.isArray(result.nodes)?result.nodes:[];computersLoaded=true;computersMessage='';}
  catch(error){computersMessage=`Saved devices could not be read: ${failure(error)}`;}
  finally{computersBusy=false;renderDevices();renderComputerFilter();}
}
async function changeComputer(args,success){
  if(computersBusy)return false;computersBusy=true;renderDevices();
  try{await command(args);const result=await command(['node','list']);computers=Array.isArray(result.nodes)?result.nodes:[];computersLoaded=true;computersMessage=success;await refresh();return true;}
  catch(error){computersMessage=failure(error);return false;}
  finally{computersBusy=false;renderDevices();renderComputerFilter();}
}
async function installComputer(id,label,cuda){
  if(computersBusy)return false;computersBusy=true;computersMessage=`${cuda?'Setting up GPU tools':'Installing RTXBridge'} on ${label}…`;renderDevices();
  try{const result=requireOk(await invoke('bootstrap_node',{id,cuda}));const preparing=[result.runtime_state,result.cuda_state].some(state=>['scheduled','building'].includes(state));computersMessage=`Bridge installed on ${label}. ${preparing?'Its tools are still being prepared; jobs will wait until they are ready.':'It is ready for jobs.'}`;await refresh();return true;}
  catch(error){computersMessage=`Setup on ${label} did not complete: ${failure(error)}`;return false;}
  finally{computersBusy=false;renderDevices();}
}
$('refresh-computers').addEventListener('click',async()=>{await readComputers();await refresh();});
function renderComputerFilter(){
  const labels=new Map(currentDevices().map(row=>[row.id,row.name]));
  for(const job of boardJobs){const id=jobRunner(job);if(!labels.has(id))labels.set(id,jobRunnerLabel(job));}
  const select=$('job-device-filter'),options=[['','All devices'],...labels];
  select.replaceChildren(...options.map(([value,label])=>{const option=element('option',null,label);option.value=value;return option;}));
  if(!options.some(([id])=>id===jobDevice))jobDevice='';select.value=jobDevice;
}

// Verify the host identity before a password can authorize this laptop's key.
const wizard={mode:'device',step:1,id:'',name:'',host:'',user:'',publicKey:'',fingerprint:'',probe:null,busy:false,done:false,autoName:false};
function wizardOs(){return document.querySelector('input[name="wizard-os"]:checked')?.value||'windows';}
function renderWizardCommand(){
  if(!wizard.publicKey)return;const os=wizardOs();$('wizard-command').textContent=deviceSetupCommand(os,wizard.publicKey);
  $('wizard-allow-intro').textContent=os==='windows'?`On ${wizard.name}, open PowerShell as administrator (Start, type PowerShell, right-click it, Run as administrator), paste this and press Enter. The first part can take a minute.`:`On ${wizard.name}, open a terminal, paste this and press Enter. It may ask for your password once.`;
}
function showWizardStep(step){
  wizard.step=step;document.querySelectorAll('[data-wizard]').forEach(el=>el.hidden=Number(el.dataset.wizard)!==step);
  document.querySelectorAll('.wizard-steps li').forEach(li=>{const n=Number(li.dataset.step);li.dataset.state=n<step?'done':n===step?'current':'';});
  $('wizard-step-label').textContent=`STEP ${step} OF 3`;$('wizard-back').hidden=step===1||wizard.done;
  const summary=wizard.probe&&probeSummary(wizard.probe);
  const manual=!$('wizard-password').value&&$('wizard-manual').open;
  $('wizard-next').textContent=step===1?'Next':step===2?(manual?`Check ${wizard.name}`:'Sign in'):wizard.done?'Done':summary?.needsWsl?'Check again':wizard.mode==='spark'?'Use as my Spark':`Add ${wizard.name}`;
}
function wizardSay(text,warn=false){$('wizard-message').textContent=text||'';$('wizard-message').dataset.warn=String(warn);}
async function scanForDevices(){
  const note=$('wizard-scan-note');note.textContent='Looking for computers on your network…';$('wizard-found').replaceChildren();
  try{
    const result=await command(['node','scan']),rows=currentDevices().filter(row=>row.kind==='computer'),names=new Set(rows.map(row=>row.name.toLowerCase())),sparkHosts=new Set(wizard.mode==='spark'?[]:savedConfig?.hosts||[]);
    const found=(Array.isArray(result.candidates)?result.candidates:[]).filter(item=>!sparkHosts.has(item.address));
    $('wizard-found').replaceChildren(...found.map(item=>{
      const added=!!item.name&&names.has(String(item.name).toLowerCase()),button=element('button','found-device');button.type='button';button.setAttribute('role','listitem');button.disabled=added;
      button.append(deviceIcon('computer'),element('strong',null,item.name||item.address),element('small',null,added?'Already added':item.name?item.address:'No name reported'));
      button.addEventListener('click',()=>{$('wizard-host').value=item.address;if(!$('wizard-name').value||wizard.autoName){$('wizard-name').value=item.name||'';wizard.autoName=true;}document.querySelectorAll('.found-device').forEach(el=>el.setAttribute('aria-pressed',String(el===button)));});
      return button;
    }));
    note.textContent=wizard.mode==='spark'?(found.length?"Your Spark is one of these. Don't see it? Type its name or address below.":"Nothing here answered yet. Type your Spark's name or address below."):found.length?"These computers already have remote access turned on. Don't see yours? Type its name below; you'll turn remote access on in the next step.":"No other computer here has remote access turned on yet. Type the computer's name below; you'll turn it on in the next step.";
  }catch(error){note.textContent=`Couldn't look around this network (${failure(error)}). Type the computer's name below.`;}
}
function openWizard(mode='device'){
  Object.assign(wizard,{mode,step:1,id:'',name:'',host:'',user:'',publicKey:'',fingerprint:'',probe:null,busy:false,done:false,autoName:false});
  for(const id of ['wizard-host','wizard-name','wizard-user','wizard-password'])$(id).value='';
  $('wizard-confirm-fingerprint').checked=false;$('wizard-password').disabled=true;$('wizard-signin-fingerprint').textContent='';
  const spark=mode==='spark';$('wizard-title').textContent=spark?'Connect your Spark':'Add a device';$('wizard-name-field').hidden=spark;$('wizard-manual').open=false;
  $('wizard-choose-intro').textContent=spark?'Pick your Spark from the computers RTXBridge found on your network, or type its name or address.':'Pick a computer RTXBridge found on your network, or type its name.';
  if(spark){$('wizard-name').value='Spark';$('wizard-user').value=savedConfig?.user||'';}
  $('wizard-progress').hidden=true;$('wizard-progress').replaceChildren();$('wizard-wsl').hidden=true;wizardSay('');showWizardStep(1);$('add-device-dialog').showModal();scanForDevices();
}
function renderWizardFound(){
  const summary=probeSummary(wizard.probe);$('wizard-found-intro').textContent=`RTXBridge reached ${summary.name} and signed in as ${wizard.user}. Is this the computer you meant?`;
  $('wizard-facts').replaceChildren(...summary.facts.map(fact=>element('li',null,fact)));$('wizard-fingerprint').textContent=wizard.probe.fingerprint;$('wizard-wsl').hidden=!summary.needsWsl;
}
function progressItem(text,state){const item=element('li',null,text);item.dataset.state=state;$('wizard-progress').append(item);return item;}
async function wizardNext(){
  if(wizard.busy)return;
  if(wizard.done){$('add-device-dialog').close();selectedDevice=wizard.id;view('devices');if(wizard.id==='spark')readPairState();return;}
  wizard.busy=true;$('wizard-next').disabled=true;wizardSay('');
  try{
    if(wizard.step===1){
      const host=$('wizard-host').value.trim(),name=($('wizard-name').value.trim()||host);
      if(!host)throw new Error("Enter the computer's name or address, or pick one above.");
      wizard.host=host;wizard.name=wizard.mode==='spark'?'your Spark':name;wizard.id=wizard.mode==='spark'?'spark':deviceSlug(name,currentDevices().map(row=>row.id));
      wizard.publicKey=(await command(['node','key',wizard.id])).public_key;renderWizardCommand();
      const identity=await command(['node','fingerprint','--host',wizard.host]);
      if(!identity.fingerprint)throw new Error("The device's identity could not be read; no password was sent.");
      wizard.fingerprint=identity.fingerprint;$('wizard-signin-fingerprint').textContent=wizard.fingerprint;
      $('wizard-confirm-fingerprint').checked=false;$('wizard-password').value='';$('wizard-password').disabled=true;
      $('wizard-signin-intro').textContent=`Sign in to ${wizard.name} once with its account and password.`;
      showWizardStep(2);$('wizard-user').focus();
    }else if(wizard.step===2||(wizard.step===3&&probeSummary(wizard.probe)?.needsWsl)){
      const user=$('wizard-user').value.trim(),password=$('wizard-password').value;
      if(!user)throw new Error(`Enter the account you sign in to ${wizard.name} with.`);
      if(!$('wizard-confirm-fingerprint').checked||!wizard.fingerprint)throw new Error('Compare the fingerprint with the device and confirm it before sending a password or continuing.');
      if(wizard.step===2&&!password&&!$('wizard-manual').open)throw new Error(`Enter ${wizard.name}'s password, or use the setup command under "Can't use a password?".`);
      wizard.user=user;
      if(wizard.step===2&&password){
        wizardSay(`Signing in to ${wizard.name}…`);
        try{requireOk(await invoke('sign_in',{id:wizard.id,host:wizard.host,user,port:22,fingerprint:wizard.fingerprint,password}));}finally{$('wizard-password').value='';}
      }
      wizardSay(`Checking ${wizard.name}…`);
      const probe=await command(['node','probe',wizard.id,'--host',wizard.host,'--user',user]);
      if(probe.fingerprint!==wizard.fingerprint){$('wizard-confirm-fingerprint').checked=false;$('wizard-password').disabled=true;throw new Error('The device identity changed. Go back and verify its fingerprint again; the new identity was not trusted.');}
      if(probe.login!=='ok'){wizardSay(probe.message||`${wizard.name} didn't let this laptop in yet.`,true);return;}
      if(wizard.mode==='spark'&&probe.os!=='linux'){wizardSay(`That computer runs ${probe.os==='windows'?'Windows':'something other than Linux'}, so it isn't a Spark. Add it as a device instead.`,true);return;}
      wizard.probe=probe;renderWizardFound();showWizardStep(3);wizardSay('');
    }else if(wizard.mode==='spark'){
      $('wizard-back').hidden=true;$('wizard-progress').hidden=false;$('wizard-progress').replaceChildren();
      const saving=progressItem('Saving your Spark','working');
      await command(['node','trust','spark','--label','Spark','--host',wizard.probe.address,'--user',wizard.user,'--fingerprint',wizard.probe.fingerprint]);
      saving.dataset.state='done';wizard.done=true;observation=null;await loadPreferences();await refresh();
      if(connectionState().action==='repair'||!observation?.devices?.find(device=>device.id==='spark')?.scheduler){
        const installing=progressItem('Installing RTXBridge on your Spark','working');
        try{requireOk(await invoke('bootstrap',{remoteAccess:false}));installing.dataset.state='done';await refresh();}
        catch(error){installing.dataset.state='failed';wizardSay(`Your Spark is saved, but installing RTXBridge on it didn't finish: ${failure(error)}. You can retry with Install or repair on its page.`,true);return;}
      }
      wizardSay('Your Spark is connected. Agents can now send it jobs.');
    }else{
      const summary=probeSummary(wizard.probe),wsl=summary.wsl;
      $('wizard-back').hidden=true;$('wizard-progress').hidden=false;$('wizard-progress').replaceChildren();
      const saving=progressItem(`Trusting ${wizard.name} and saving it`,'working');
      await command(['node','trust',wizard.id,'--label',wizard.name,'--host',wizard.probe.address,'--user',wizard.user,'--fingerprint',wizard.probe.fingerprint,...(wsl?['--wsl-distro',wsl.distribution,'--wsl-user',wsl.user]:[])]);
      saving.dataset.state='done';wizard.done=true;await readComputers();
      const installing=progressItem(`Installing RTXBridge on ${wizard.name}`,'working');
      if(!await installComputer(wizard.id,wizard.name,false)){installing.dataset.state='failed';wizardSay(`${wizard.name} is added, but installing RTXBridge didn't finish: ${computersMessage}. You can retry from its page.`,true);return;}
      installing.dataset.state='done';
      if(summary.hasGpu){const gpu=progressItem('Setting up GPU tools','working');gpu.dataset.state=await installComputer(wizard.id,wizard.name,true)?'done':'failed';if(gpu.dataset.state==='failed')wizardSay(`GPU tools didn't finish setting up: ${computersMessage}. You can retry from ${wizard.name}'s page.`,true);}
      if($('wizard-message').dataset.warn!=='true')wizardSay(`${wizard.name} is ready. Agents can now send it jobs.`);
    }
  }catch(error){$('wizard-progress').querySelectorAll('[data-state=working]').forEach(item=>item.dataset.state='failed');if(!wizard.done)$('wizard-back').hidden=false;wizardSay(failure(error),true);}
  finally{wizard.busy=false;$('wizard-next').disabled=false;showWizardStep(wizard.step);}
}
$('add-device').addEventListener('click',()=>openWizard('device'));
for(const id of ['connect-spark','ssh-use-password'])$(id).addEventListener('click',()=>openWizard('spark'));
document.querySelectorAll('input[name="wizard-os"]').forEach(input=>input.addEventListener('change',renderWizardCommand));
$('wizard-password').addEventListener('input',()=>showWizardStep(wizard.step));$('wizard-manual').addEventListener('toggle',()=>showWizardStep(wizard.step));
$('wizard-confirm-fingerprint').addEventListener('change',()=>{const confirmed=$('wizard-confirm-fingerprint').checked;$('wizard-password').disabled=!confirmed;if(!confirmed)$('wizard-password').value='';});
$('wizard-password').addEventListener('keydown',event=>{if(event.key==='Enter')wizardNext();});
$('add-device-dialog').addEventListener('close',()=>{$('wizard-password').value='';$('wizard-password').disabled=true;$('wizard-confirm-fingerprint').checked=false;});
$('wizard-next').addEventListener('click',wizardNext);
$('wizard-back').addEventListener('click',()=>{if(!wizard.busy&&wizard.step>1){wizardSay('');showWizardStep(wizard.step-1);}});
$('close-wizard').addEventListener('click',()=>$('add-device-dialog').close());
$('wizard-name').addEventListener('input',()=>{wizard.autoName=false;});
$('copy-wizard-command').addEventListener('click',()=>copyText($('wizard-command').textContent,'Setup command'));
$('job-device-filter').addEventListener('change',event=>{jobDevice=event.target.value;pageIndex=0;renderJobs();});

// Jobs
function renderBoard(){
  const summary=boardSummary(observation,boardJobs),state=connectionState(),active=summary.sparkRunning+summary.sparkQueued+summary.laptopActive+summary.remoteActive;
  $('activity-label').textContent=(health.failures?'Last seen: ':'')+(active?`${active} active ${active===1?'job':'jobs'}`:health.failures?'no active jobs':observation?.ok===false?'Jobs unavailable':'No active jobs');$('activity-dot').dataset.active=String(active>0);
  $('nav-active').hidden=!active;$('nav-active').textContent=active>9?'9+':String(active);
  renderDeviceStrip();
  renderComputerFilter();renderJobs();
}
function stateLabel(state){return state?state[0].toUpperCase()+state.slice(1):'Unknown';}
function renderJobs(){
  const filtered=filterJobs(boardJobs,{filter:jobFilter,query:jobQuery,device:jobDevice}),page=jobPage(filtered,pageIndex,PAGE_SIZE);pageIndex=page.page;
  $('job-count').textContent=String(boardJobs.length);
  const list=$('jobs');list.replaceChildren();
  $('empty-jobs').hidden=page.total>0;$('empty-jobs-text').textContent=boardJobs.length?'No jobs match this filter.':observation?.ok===false?'The job board could not be read. Check the Spark connection.':'No jobs recorded yet. Ask an agent to run a build.';
  $('pagination').hidden=page.pages<2;$('page-summary').textContent=`${page.total?page.start+1:0}–${Math.min(page.start+PAGE_SIZE,page.total)} of ${page.total} jobs`;$('page-position').textContent=`Page ${page.page+1} of ${page.pages}`;$('previous-page').disabled=page.page===0;$('next-page').disabled=page.page===page.pages-1;
  const now=Number(observation?.observed_at)||Date.now()/1000;
  for(const job of page.rows){
    const id=jobId(job),outcome=jobOutcome(job),runner=jobRunner(job),flags=jobFlags(job,now);
    const item=element('li');const button=element('button','job-row');button.type='button';button.dataset.job=id;button.dataset.outcome=outcome.kind;if(id===selectedJob)button.setAttribute('aria-current','true');
    button.setAttribute('aria-label',`Job ${id}, ${clientLabel(job.client)}, ${jobRunnerLabel(job)}, ${job.state||'unknown'}, ${outcome.label}${job.ui_observation_stale?', last seen':''}`);
    const top=element('span','job-row-top');const state=element('span','job-state');state.dataset.state=job.state||'unknown';state.title=stateLabel(job.state);top.append(state,element('span','job-id',job.kind==='service'?serviceView(job).name:id.slice(0,12)||'—'),element('span','job-time',relativeTime(job.created_at,now)));
    const bottom=element('span','job-row-meta');const who=element('span',`client-tag client-${job.client||'unknown'}`,clientLabel(job.client));bottom.append(who,element('span','runner-tag',jobRunnerLabel(job)));if(job.kind==='service')bottom.append(element('span','service-name',`Model server · ${serviceView(job).state}`));else if(job.runtime)bottom.append(element('span','meta-text',jobToolsLabel(job)));
    const result=element('span',`outcome outcome-${outcome.kind}`,outcome.label);bottom.append(result);if(job.ui_observation_stale)bottom.append(element('span','meta-text','Last seen'));
    if(flags.some(flag=>flag.kind==='warn')){const warn=element('span','row-warn','!');warn.title=flags.filter(flag=>flag.kind==='warn').map(flag=>flag.label).join(', ');bottom.append(warn);}
    button.append(top,bottom);button.addEventListener('click',()=>selectJob(id));item.append(button);list.append(item);
  }
  renderJobDetail();
}
$('previous-page').addEventListener('click',()=>{pageIndex--;renderJobs();});$('next-page').addEventListener('click',()=>{pageIndex++;renderJobs();});
document.querySelectorAll('[data-filter]').forEach(button=>button.addEventListener('click',()=>{jobFilter=button.dataset.filter;pageIndex=0;document.querySelectorAll('[data-filter]').forEach(el=>el.setAttribute('aria-pressed',String(el===button)));renderJobs();}));
$('job-search').addEventListener('input',event=>{jobQuery=event.target.value;pageIndex=0;renderJobs();});
document.addEventListener('keydown',event=>{if((event.ctrlKey||event.metaKey)&&event.key.toLowerCase()==='k'){event.preventDefault();view('overview');$('job-search').focus();}if(event.key==='F5'){event.preventDefault();refresh();}if(event.key==='Escape'&&selectedJob&&currentView==='overview'&&!document.querySelector('dialog[open]'))selectJob(null);});
function selectJob(id){if(selectedJob!==id){selectedJob=id;cancelArmed=null;stopLogFollow();logState=id?(logCache.get(id)||{job:id,text:'',offset:0,loading:false,error:'',decoder:new TextDecoder()}):null;if(id)logCache.set(id,logState);}renderJobs();if(id)loadLogs(false);}
$('close-job').addEventListener('click',()=>selectJob(null));
function renderJobDetail(){
  const job=boardJobs.find(item=>jobId(item)===selectedJob)||jobCache.get(selectedJob);
  $('job-welcome').hidden=!!job;$('job-view').hidden=!job;
  if(!job){if(selectedJob&&boardJobs.length){/* Selected job left the board; keep its retained output visible. */}return;}
  const now=Number(observation?.observed_at)||Date.now()/1000,outcome=jobOutcome(job),runner=jobRunner(job);
  $('detail-state').dataset.state=job.state||'unknown';$('detail-state').textContent=serviceView(job)?.state||stateLabel(job.state);$('detail-id').textContent=serviceView(job)?`Model server · ${serviceView(job).name}`:jobId(job);
  const flags=$('detail-flags');flags.replaceChildren(...jobFlags(job,now).map(flag=>{const node=element('div',`flag flag-${flag.kind}`);node.append(element('strong',null,flag.label),element('span',null,flag.detail));return node;}));
  const started=Number(job.started_at),finished=Number(job.finished_at),cpu=job.reserved_cpu??job.cpu,memory=job.reserved_memory_gb??job.memory_gb;
  const service=serviceView(job);
  const facts=[['Result',service?service.state:outcome.label,`fact-outcome outcome-${outcome.kind}`],['Submitted by',`${clientLabel(job.client)} (self-declared)`],['Runs on',jobRunnerLabel(job,true)],['Tools',jobToolsLabel(job)],['Set aside',cpu!=null?`${cpu} cores · ${memory??'—'} GiB`:runner==='laptop'?'No enforced limits':'Not reported'],['Internet',jobInternet(job)],['Downloaded',formatGiB(job.internet_use?.bytes_down)],['Queued',relativeTime(job.created_at,now)],['Run time',Number.isFinite(started)&&started>0?duration((Number.isFinite(finished)&&finished>0?finished:now)-started)+(isActiveJob(job)?' so far':''):'Not started']];
  if(job.ui_observation_stale)facts.push(['Observation','Last seen; this computer did not provide a fresh job list.']);
  if(job.return_status==='complete')facts.push(['Results',`Results returned to ${job.results_folder}`]);
  else if(job.return_status==='retrying')facts.push(['Results',job.return_error||'Return paused; the bridge will retry.']);
  else if(job.state==='returning')facts.push(['Results','Returning and verifying files on this laptop...']);
  if(job.returned?.some(file=>file.status==='conflict'))facts.push(['Local edits','Preserved. Spark versions were saved beside conflicting files.']);
  renderFacts($('detail-facts'),facts);
  const more=[['Job ID',jobId(job)],['Request ID',job.request_id||'Not reported'],['Reported state',job.state||'Unknown']];if(job.cached_from)more.push(['Reused job',String(job.cached_from)]);if(job.error)more.push(['Reported error',typeof job.error==='string'?job.error:job.error.message||job.error.code]);if(job.internet_reason)more.push(['Internet report',job.internet_reason]);if(job.stop_reason)more.push(['Stop reason',job.stop_reason]);renderFacts($('detail-more'),more);
  $('service-note').hidden=!service;$('service-note').textContent=service?[service.reason,service.note].filter(Boolean).join('. '):'';
  const cancellable=isActiveJob(job)&&job.state!=='returning'&&!job.cancel_requested;$('cancel-job').hidden=!cancellable;$('cancel-job').querySelector('span').textContent=cancelArmed===jobId(job)?'Confirm cancel':service?'Stop server':'Cancel job';$('cancel-job').classList.toggle('armed',cancelArmed===jobId(job));
  renderLog();
  if(isActiveJob(job))startLogFollow();else stopLogFollow();
}
function renderLog(){if(!logState)return;$('job-output').textContent=logState.text||(logState.loading?'Loading retained output…':'No output recorded yet.');$('log-meta').dataset.error=String(!!logState.error);$('log-meta').textContent=logState.error?"Couldn't fetch new output. Retrying.":`${(logState.offset/1024).toFixed(1)} KiB received${logTimer?' · following':''}`;}
async function loadLogs(){
  const state=logState;if(!state||state.loading)return;state.loading=true;state.error='';renderLog();
  try{const result=await command(['logs',state.job,'--since',String(state.offset)]);const bytes=typeof result.data==='string'?Uint8Array.from(atob(result.data),c=>c.charCodeAt(0)):new TextEncoder().encode(result.text||'');const next=Number.isSafeInteger(result.next_offset)?result.next_offset:state.offset+bytes.length;if(next<state.offset||next-state.offset!==bytes.length)throw new Error('Log offset did not match the returned output.');state.text+=state.decoder.decode(bytes,{stream:true});state.offset=next;if(state.text.length>2*1024*1024)state.text='[Earlier output is retained by the bridge.]\n'+state.text.slice(-2*1024*1024);const pre=$('job-output'),atEnd=pre.scrollHeight-pre.scrollTop-pre.clientHeight<40;state.loading=false;if(logState===state){renderLog();if(atEnd)pre.scrollTop=pre.scrollHeight;}}
  catch(error){state.loading=false;state.error=failure(error);if(logState===state)renderLog();}
}
function startLogFollow(){if(!logTimer)logTimer=setInterval(()=>{if(!document.hidden&&currentView==='overview')loadLogs(false);},4000);}
function stopLogFollow(){clearInterval(logTimer);logTimer=null;}
$('refresh-output').addEventListener('click',()=>loadLogs(false));
$('copy-job-id').addEventListener('click',()=>copyText(selectedJob||'','Job ID'));
$('cancel-job').addEventListener('click',()=>busy($('cancel-job'),async()=>{const id=selectedJob;if(!id)return;if(cancelArmed!==id){cancelArmed=id;renderJobDetail();setTimeout(()=>{if(cancelArmed===id){cancelArmed=null;renderJobDetail();}},5000);return;}cancelArmed=null;await command(['cancel',id]);message('Cancellation requested. The runner reports the final state.');await refresh();}));

// Preferences, connection forms and pairing
function resourceTarget(config){return JSON.stringify(config?[config.ssh_alias||'',config.host_key_alias||'',config.user||'',config.port||22,config.hosts||[]]:null);}
async function loadPreferences(fillForm=true){const prefs=await invoke('preferences'),targetChanged=resourceTarget(savedConfig)!==resourceTarget(prefs.config);preferences=prefs;savedConfig=prefs.config||null;if(targetChanged){observation=null;boardJobs=[];selectJob(null);resourceGeneration++;extrasEpoch++;internet=null;downloads=null;downloadsDirty=false;clearArmed=false;internetMessage='';downloadsMessage='';lastGoodObservation=null;lastFailure=null;health={failures:0,lastGoodAt:0};resourceReady=false;resourceHardware=null;if(limits){$('resource-observation').textContent='Spark connection changed. Your draft is kept; read settings before saving to this Spark.';renderResourceDraft();}}laptop=prefs.laptop||laptop;const version=prefs.app_version||'0.4.2';$('app-version').textContent=version;$('installed-version').textContent=version;if(prefs.error)message(prefs.error.message||prefs.error);if(savedConfig){if(prefs.connection){savedConfig.connection_mode=prefs.connection.mode;savedConfig.connection_enabled=prefs.connection.enabled;}if(fillForm){const form=$('ssh-form');for(const [field,key] of [['host','ssh_alias'],['hostKeyAlias','host_key_alias'],['user','user'],['identity_file','identity_file'],['local_build_gate','local_build_gate']])form.elements[field].value=savedConfig[key]||'';form.elements.hostname.value=savedConfig.hosts?.[0]||'';form.elements.otherHosts.value=(savedConfig.hosts||[]).slice(1).join(', ');form.elements.port.value=savedConfig.port||22;$('mode-form').elements.mode.value=configMode(savedConfig);$('auto-addresses').checked=savedConfig.auto_addresses!==false;$('bootstrap-remote').checked=savedConfig.allow_remote===true;}}else $('ssh-settings').open=true;renderLaptop();renderConnection();}
function renderLaptop(){if(Number.isSafeInteger(laptop?.concurrency_limit)){$('laptop-limit').value=laptop.concurrency_limit;$('laptop-limit').max=Math.min(64,laptop.logical_parallelism||64);$('laptop-limit').disabled=false;$('save-laptop').disabled=false;}}
$('connection-toggle').addEventListener('click',()=>busy($('connection-toggle'),async()=>{connecting=true;clearFind();renderConnection();try{const response=await invoke('connection',{mode:null,enabled:savedConfig?.connection_enabled===false,laptopJobs:null});await loadPreferences(false);requireOk(response);await refresh();message(savedConfig?.connection_enabled===false?'Disconnected. Admitted jobs keep running.':'Saved connection checked.');}finally{connecting=false;renderConnection();}}));
$('mode-form').addEventListener('submit',event=>{event.preventDefault();busy(event.submitter,async()=>{requireOk(await invoke('connection',{mode:new FormData(event.target).get('mode'),enabled:null,laptopJobs:null,autoAddresses:$('auto-addresses').checked}));observation=null;connecting=true;clearFind();try{await loadPreferences(false);await refresh();}finally{connecting=false;renderConnection();}message('Connection mode saved. Your pairing is unchanged.');});});
$('ssh-form').addEventListener('submit',event=>{event.preventDefault();busy(event.submitter,async()=>{const form=new FormData(event.target),host=form.get('host').trim(),hostname=form.get('hostname').trim();const args=['setup','--ssh-alias',host,'--host-key-alias',form.get('hostKeyAlias').trim()||host,'--port',form.get('port')];if(hostname)args.push('--host',hostname);for(const other of otherLanAddresses(form.get('otherHosts'),hostname))args.push('--host',other);for(const [field,flag] of [['user','--user'],['identity_file','--identity-file'],['local_build_gate','--local-build-gate']]){const value=form.get(field).trim();if(value)args.push(flag,value);}if(configMode(savedConfig)!=='lan-only')args.push('--allow-remote');if(args.length>24)throw new Error('Too many LAN addresses to save at once. Remove some from Other LAN addresses.');await command(args);clearFind();await loadPreferences();message('Connection saved. Choose Test connection when ready; Spark services were not restarted.');});});
$('test-connection').addEventListener('click',()=>busy($('test-connection'),async()=>{const result=await command(['doctor']);await refresh();message(result.checks?.docker===false?"Connected, but Docker isn't running on the Spark.":result.scheduler?.healthy?'Connection verified. Docker and the job service are ready.':'Connected, but the Spark job service is not responding.');}));
$('bootstrap').addEventListener('click',()=>busy($('bootstrap'),async()=>{$('bootstrap-message').textContent='Installing the independent bridge over pinned SSH…';try{const result=requireOk(await invoke('bootstrap',{remoteAccess:$('bootstrap-remote').checked}));$('bootstrap-message').textContent=[result.runtime_state,result.cuda_state].some(state=>['scheduled','building'].includes(state))?'Bridge installed. Its tools are queued or preparing; check status before running jobs.':'Bridge installed. Check its connection and tools before running jobs.';await refresh();}catch(error){$('bootstrap-message').textContent=failure(error);throw error;}}));

// Find my Spark: owner-started only. It never runs from refresh, startup or an error.
function renderFindControls(){const control=findSparkControl({config:savedConfig,connection:connectionState(),connecting,busy:findBusy}),button=$('find-spark');button.disabled=control.disabled;button.className=`button ${control.suggested?'primary':'secondary'}`;button.dataset.suggested=String(control.suggested);if(control.reason&&!findBusy)clearFind();for(const use of $('find-results').querySelectorAll('button'))use.disabled=control.disabled;if(!findBusy)$('find-message').textContent=findMessage??(control.reason||FIND_INTRO);}
function clearFind(){findMessage=null;$('find-results').replaceChildren();$('find-results').hidden=true;}
function renderFind(view){
  findMessage=view.summary;$('find-message').textContent=view.summary;
  const list=$('find-results');list.replaceChildren();
  for(const row of view.rows){const item=element('li');const text=element('span','find-host');text.append(element('strong',null,row.host+(row.recommended?' · Recommended':'')),element('small',null,row.detail));item.append(text);if(!row.alreadyFirst){const use=element('button','button secondary small','Use this address');use.type='button';use.addEventListener('click',()=>useFoundAddress(row.host,use));item.append(use);}list.append(item);}
  for(const note of [view.emptyReason,...view.notes].filter(Boolean))list.append(element('li','find-note',note));
  list.hidden=!list.childElementCount;
}
$('find-spark').addEventListener('click',()=>busy($('find-spark'),async()=>{findBusy=true;clearFind();$('find-message').textContent='Searching… up to 25 seconds';try{renderFind(discoveryView(await command(['discover']),savedConfig));}catch(error){findMessage=failure(error);}finally{findBusy=false;}}).then(renderFindControls));
async function useFoundAddress(host,button){
  if(findBusy)return;
  await busy(button,async()=>{findBusy=true;renderFindControls();$('find-message').textContent=`Checking ${host} again with your pinned host key…`;
    try{const result=await command(['discover','--save',host]);clearFind();findMessage=discoverySaved(result,host);await loadPreferences(true);message(findMessage);await refresh();}
    catch(error){findMessage=failure(error);}finally{findBusy=false;}});
  renderFindControls();
}

// Resources
// These controls show the saved Spark setting. A switch never claims that a download succeeded.
function renderExtras(){
  const iv=internetView(internet),dv=downloadsView(downloads),connected=connectionState().connected;
  $('internet-summary').textContent=iv.label;$('internet-status').textContent=!connected&&iv.known?'Last observed setting. Reconnect and refresh to check it.':internetMessage||iv.note;
  $('internet-switch').setAttribute('aria-checked',String(iv.enabled===true));$('internet-switch').setAttribute('aria-busy',String(internetBusy));
  $('internet-switch').disabled=!iv.known||!connected||resourceBusy||downloadsBusy;
  $('internet-note').textContent=INTERNET_COPY.note;$('internet-blocked').textContent=INTERNET_COPY.blocked;
  $('internet-limitation').textContent=INTERNET_COPY.limitation;$('internet-rule').textContent=INTERNET_COPY.rule;
  $('downloads-summary').textContent=dv.label;$('downloads-status').textContent=!connected&&dv.known?'Last observed downloads. Reconnect and refresh to check them.':downloadsMessage||dv.note;
  $('downloads-note').textContent='Free up space removes reusable downloads when Spark jobs are idle. Jobs may download them again. It does not remove projects or job logs.';
  if(!downloadsDirty&&dv.keepGiB!==null&&dv.keepGiB!==undefined)$('downloads-limit').value=dv.keepGiB;
  $('downloads-limit').disabled=!dv.known||!connected||downloadsBusy||resourceBusy;
  $('save-downloads').disabled=!dv.known||!connected||downloadsBusy||resourceBusy||!downloadsDirty;
  $('refresh-downloads').disabled=!connected||downloadsBusy||resourceBusy;
  $('clear-downloads').disabled=!dv.known||!connected||downloadsBusy||resourceBusy||dv.clearPending;
  $('confirm-clear-downloads').hidden=!clearArmed;$('confirm-clear-downloads').disabled=downloadsBusy||resourceBusy||!connected;
  renderFacts($('downloads-facts'),dv.facts);
  $('download-items').replaceChildren(...(Array.isArray(downloads?.items)?downloads.items.slice(0,20):[]).map(item=>{
    const line=element('li');line.append(element('span',null,String(item.path||item.name||'Saved file')),element('span',null,formatGiB(item.bytes??item.size_bytes)));return line;
  }));
}
try{$('extras-new').hidden=localStorage.getItem('dgxbridge-extras-seen')==='1';}catch{}
$('dismiss-extras').addEventListener('click',()=>{$('extras-new').hidden=true;try{localStorage.setItem('dgxbridge-extras-seen','1');}catch{}});
$('internet-switch').addEventListener('click',async()=>{
  const iv=internetView(internet);if(resourceBusy||downloadsBusy||!iv.known||!connectionState().connected)return;
  const generation=resourceGeneration;resourceBusy=true;internetBusy=true;extrasEpoch++;internetMessage='Saving the Spark setting…';renderExtras();renderResourceDraft();
  try{
    const result=requireOk(await invoke('resources',{policy:null,internet:!iv.enabled}));
    if(generation!==resourceGeneration)return;acceptExtras(result);internetMessage='Saved and read back from your Spark.';
  }catch(error){
    if(generation!==resourceGeneration)return;
    internetMessage="Your Spark didn't confirm the change. Checking what it has now…";renderExtras();
    try{const result=requireOk(await invoke('resources',{policy:null,internet:null}));if(generation!==resourceGeneration)return;acceptExtras(result);internetMessage='Showing the setting read back from your Spark.';}
    catch(readError){if(generation===resourceGeneration)internetMessage=`The saved setting could not be checked. ${failure(readError)} Last observed setting is shown; use Refresh settings before trying again.`;}
  }finally{resourceBusy=false;internetBusy=false;renderExtras();renderResourceDraft();renderConnection();}
});
async function changeDownloads(args){
  if(downloadsBusy||resourceBusy||!connectionState().connected)return;
  const generation=resourceGeneration;downloadsBusy=true;extrasEpoch++;downloadsMessage=args.length>1?'Waiting for the Spark to confirm…':'Checking saved downloads…';renderExtras();
  try{const result=await command(args);if(generation!==resourceGeneration)return;acceptExtras(result);if(args.includes('--keep-gb'))downloadsDirty=false;downloadsMessage=args.includes('--free-up')?(downloadsView(downloads).clearPending?'Clearing is queued until Spark jobs are idle.':'The Spark processed the cleanup request. See its reported space below.'):'Saved downloads checked.';clearArmed=false;}
  catch(error){if(generation===resourceGeneration)downloadsMessage=`${failure(error)}${args.length>1?' The change was not confirmed. Refresh before trying again.':''}`;}
  finally{downloadsBusy=false;renderExtras();renderConnection();}
}
$('refresh-downloads').addEventListener('click',()=>changeDownloads(['downloads']));
$('downloads-limit').addEventListener('input',()=>{downloadsDirty=true;renderExtras();});
$('downloads-form').addEventListener('submit',event=>{event.preventDefault();const keep=Number($('downloads-limit').value);if(!Number.isSafeInteger(keep)||keep<20){downloadsMessage='Choose a whole number of at least 20 GiB. The Spark also checks available disk capacity.';renderExtras();return;}changeDownloads(['downloads','--keep-gb',String(keep)]);});
$('clear-downloads').addEventListener('click',()=>{clearArmed=!clearArmed;renderExtras();});
$('confirm-clear-downloads').addEventListener('click',()=>changeDownloads(['downloads','--free-up']));
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
  $('memory-budget-note').textContent=limits&&memory?`${configured} build budget · ${headroom} GiB kept free.`:'Headroom stays outside the build budget.';
  $('cpu-budget-bar').setAttribute('aria-label',limits&&cores?`${configured} CPU budget ${cpuBudget} of ${cores} hardware cores. Not live usage.`:'CPU budget not observed');
  $('memory-budget-bar').setAttribute('aria-label',limits&&memory?`${configured} memory budget ${memoryBudget} of ${memory} GiB hardware memory; ${headroom} GiB kept free. Not live usage.`:'Memory budget not observed');
  $('resource-draft-summary').textContent=policy?`${configured}: ${presetName} · up to ${policy.cpu_per_job} cores and ${policy.memory_gb_per_job} GiB per job · ${policy.client_jobs} ${policy.client_jobs===1?'job':'jobs'} per agent.`:limits?'Adjust the limits below to create a valid budget.':'Your saved limits will appear here.';
  $('resource-validation').textContent=error||(!limits?'Read the Spark policy before editing.':!resourceReady?'Current Spark settings need a fresh read. Your draft is kept; read settings again before saving.':resourceDirty?'Unsaved changes. CPU per job ≤ per agent ≤ total; memory budget + memory kept free fits your Spark.':'Saved limits loaded. Choose a preset or open advanced limits to make changes.');
  $('resource-validation').dataset.state=error?'error':resourceDirty?'draft':'saved';
  $('save-resources').disabled=!policy||!resourceDirty||!resourceReady||resourceBusy;
  $('discard-resources').hidden=!resourceDirty;$('discard-resources').disabled=resourceBusy;
}
async function readResources(){
  if(resourceBusy||downloadsBusy)return;const generation=resourceGeneration;resourceBusy=true;extrasEpoch++;$('refresh-resources').disabled=true;renderResourceDraft();renderExtras();
  try{const result=requireOk(await invoke('resources',{policy:null}));if(generation!==resourceGeneration)throw new Error('Spark connection changed during the request. Read settings again before saving.');const keepDraft=resourceDirty;limits=result.limits;acceptExtras(result);resourceHardware=result.hardware||resourceHardware;resourceReady=true;if(!keepDraft)fillResourceDraft(limits);$('resource-observation').textContent=keepDraft?'Latest saved policy refreshed. Your unsaved changes are kept.':'Current Spark policy loaded.';renderAgents();}
  catch(error){resourceReady=false;$('resource-observation').textContent=`${failure(error)}${limits?' Your last observed policy and draft are retained.':''}`;}
  finally{resourceBusy=false;$('refresh-resources').disabled=false;renderResourceDraft();renderExtras();}
}
$('refresh-resources').addEventListener('click',readResources);
$('resource-fields').addEventListener('input',renderResourceDraft);
$('discard-resources').addEventListener('click',()=>{if(limits){fillResourceDraft(limits);renderResourceDraft();}});
$('resources-form').addEventListener('submit',async event=>{
  event.preventDefault();if(resourceBusy||downloadsBusy||!resourceReady||$('save-resources').disabled)return;const generation=resourceGeneration;
  try{const policy=resourcePolicy(new FormData(event.target),resourceHardware);resourceBusy=true;extrasEpoch++;$('refresh-resources').disabled=true;renderResourceDraft();renderExtras();const result=requireOk(await invoke('resources',{policy}));if(generation!==resourceGeneration)throw new Error('Spark connection changed during the request. Read settings again before saving.');limits=result.limits;acceptExtras(result);resourceHardware=result.hardware||resourceHardware;resourceReady=true;fillResourceDraft(limits);$('resource-observation').textContent='Spark limits saved and read back. Running jobs retain their admitted limits.';renderAgents();}
  catch(error){$('resource-observation').textContent=`${failure(error)} Your unsaved changes are kept.`;}
  finally{resourceBusy=false;$('refresh-resources').disabled=false;renderResourceDraft();renderExtras();}
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
function renderAgents(){const state=connectionState(),paths=agentPaths(agent,preferences);$('agent-file').textContent=agent==='codex'?'Add to ~/.codex/config.toml':'Add to your project’s .mcp.json';$('agent-label').textContent=agent;$('agent-config').textContent=agentConfiguration(agent,paths.path,paths.stateRoot);$('agent-prompt').textContent=agentPrompt({agent,...paths,config:savedConfig,connection:state,limits:resourceReady?limits:null,computers:namedComputers(computerObservation||observation,computers)});document.querySelectorAll('[data-agent]').forEach(el=>{el.classList.toggle('selected',el.dataset.agent===agent);el.setAttribute('aria-pressed',String(el.dataset.agent===agent));});}
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

motionQuery.addEventListener('change',event=>{reduced=event.matches;renderConnection();renderDevices();if(reduced)cancelAnimationFrame(pairFrame);});
async function init(){try{await loadPreferences();}catch(error){message(failure(error));}await refresh();performUpdate('status',true);}
init();const refreshTimer=setInterval(()=>{if(!document.hidden&&['overview','devices'].includes(currentView))refresh();},15000);
window.addEventListener('beforeunload',()=>{clearInterval(refreshTimer);clearInterval(pairTimer);stopLogFollow();clearTimeout(updateTimer);cancelAnimationFrame(pairFrame);fleetMap?.dispose();});

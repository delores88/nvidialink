import {CONNECTION_RIBBONS,startLinkFilament} from './vendor/sparkLinkParticles.mjs';
import {linkMotion} from './vendor/sparkLinkMotion.mjs';

// PhaseForge SparkLinkVisuals hardware geometry, pinned at
// 7a665907901bd797df0eedee961e0d83a41eeabf. See PROVENANCE.md.
function hardware(spark,id) {
  return `<div class="connection-device"><svg class="hardware" viewBox="0 0 104 78" aria-hidden="true"><defs>
    <linearGradient id="${id}-metal" x1="0" y1="0" x2="1" y2="1"><stop stop-color="#9aafb8"/><stop offset="1" stop-color="#46565f"/></linearGradient>
    <linearGradient id="${id}-screen" x1="0" y1="0" x2="1" y2="1"><stop stop-color="#173e47"/><stop offset="1" stop-color="#1c2236"/></linearGradient>
    <linearGradient id="${id}-side" x1="0" y1="0" x2="1" y2="1"><stop stop-color="#415b67"/><stop offset="1" stop-color="#222d3e"/></linearGradient>
    <linearGradient id="${id}-accent"><stop stop-color="#65d9ce"/><stop offset=".5" stop-color="#74acff"/><stop offset="1" stop-color="#b798f8"/></linearGradient>
    </defs><ellipse class="device-shadow" cx="52" cy="68" rx="42" ry="5"/>
    ${spark?`<path d="M17 23 47 10 87 22 57 37Z" fill="url(#${id}-metal)" class="hardware-edge"/><path d="M17 23 57 37V66L17 52Z" fill="url(#${id}-screen)" class="hardware-edge"/><path d="M57 37 87 22V52L57 66Z" fill="url(#${id}-side)" class="hardware-edge"/>${Array.from({length:7},(_,i)=>`<path d="M${23+i*4} ${31+i*1.4}v15" class="vent"/>`).join('')}${Array.from({length:5},(_,i)=>`<path d="M${63+i*4} ${40-i*1.85}v14" class="side-vent"/>`).join('')}<path d="m41 21 11-5 13 4-11 5Z" fill="none" stroke="url(#${id}-accent)" stroke-width="1.1"/><path d="m27 51 18 6" stroke="url(#${id}-accent)" stroke-width="1.3" stroke-linecap="round"/><circle class="device-led" cx="50" cy="57" r="1.3"/>`:
    `<rect x="15" y="10" width="74" height="47" rx="5" fill="url(#${id}-metal)" class="hardware-edge"/><rect x="19" y="14" width="66" height="38" rx="2" fill="url(#${id}-screen)"/><g fill="url(#${id}-accent)">${Array.from({length:28},(_,i)=>`<rect x="${32+i%7*6}" y="${23+Math.floor(i/7)*6}" width="2" height="2" rx=".5" opacity="${.25+(i*13%7)/10}"/>`).join('')}</g><path d="M15 57H89L99 65Q100 68 95 68H9Q4 68 5 65Z" fill="url(#${id}-metal)" class="hardware-edge"/><path d="M38 58H66L69 61H35Z" fill="url(#${id}-screen)"/><path d="M12 65H92" stroke="url(#${id}-accent)" stroke-width=".8" opacity=".65"/>`}
    </svg><span class="device-label">${spark?'Your Spark':'This laptop'}</span></div>`;
}
export function createConnectionVisual(host) {
  const id=host.id.replace(/[^a-zA-Z0-9_-]/g,'');
  host.classList.add('connection-visual');host.setAttribute('role','img');
  host.innerHTML=hardware(false,`${id}-laptop`)+`<div class="connection-lane"><svg class="ribbons" viewBox="0 0 1000 100" preserveAspectRatio="none" aria-hidden="true"><defs><linearGradient id="${id}-ribbon"><stop stop-color="#65d9ce"/><stop offset=".45" stop-color="#74acff"/><stop offset=".75" stop-color="#b798f8"/><stop offset="1" stop-color="#de9cd3"/></linearGradient></defs><g stroke="url(#${id}-ribbon)">${CONNECTION_RIBBONS.map(d=>`<path d="${d}" vector-effect="non-scaling-stroke"/>`).join('')}</g></svg><canvas class="connection-particles" aria-hidden="true"></canvas><span class="path-label"></span></div>`+hardware(true,`${id}-spark`);
  const canvas=host.querySelector('canvas'),label=host.querySelector('.path-label');
  let stop=null,previous=null,reconnectedAt=null,lostAt=null,disposal=null,input={};
  const read=()=>linkMotion({...input,reconnectedAgoS:reconnectedAt===null?null:(performance.now()-reconnectedAt)/1000});
  return {
    render(connection,reduced=false) {
      const now=performance.now(),health=!connection.enabled?'paused':connection.connected?'online':'offline';
      if(previous==='online'&&health==='offline')lostAt=now;
      if(previous==='offline'&&health==='online')reconnectedAt=now;
      previous=health;
      // No throughput, latency, GPU utilization, transfer position or relay claim is synthesized.
      input={health,path:connection.connected&&connection.route==='Pinned SSH · LAN'?'lan':null,reduced};
      const snapping=health==='offline'&&lostAt!==null&&now-lostAt<1800;
      const active=!reduced&&(health==='online'||snapping);
      host.dataset.motion=active?'live':'static';host.dataset.state=health;
      host.dataset.route=input.path||'unknown';
      host.setAttribute('aria-label',`Laptop to Spark: ${connection.route}`);label.textContent=connection.route;
      if(active&&!stop)stop=startLinkFilament(canvas,read);
      if(!active&&stop){stop();stop=null;}
      clearTimeout(disposal);
      if(snapping)disposal=setTimeout(()=>{if(previous==='offline'){stop?.();stop=null;host.dataset.motion='static';}},1800);
    },
    dispose(){clearTimeout(disposal);stop?.();stop=null;},
  };
}

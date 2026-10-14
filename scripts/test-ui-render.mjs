// Isolated browser fixture only. This never calls SSH, Tauri, Marketplace or an installer.
import {createServer} from 'node:http';
import {readFile,mkdir} from 'node:fs/promises';
import {resolve,extname} from 'node:path';
import {pathToFileURL} from 'node:url';
import assert from 'node:assert/strict';
const packageRoot=process.env.DGXBRIDGE_TEST_NODE_MODULES;
if(!packageRoot)throw new Error('Set DGXBRIDGE_TEST_NODE_MODULES to the installed test dependency directory.');
const {chromium}=await import(pathToFileURL(resolve(packageRoot,'playwright/index.mjs')));
const root=resolve('ui');
const server=createServer(async(req,res)=>{try{const path=resolve(root,'.'+new URL(req.url,'http://localhost').pathname.replace(/\/$/,'/index.html'));if(!path.startsWith(root+'\\')&&!path.startsWith(root+'/'))throw new Error('Outside fixture root');const bytes=await readFile(path);res.setHeader('Content-Type',({'.html':'text/html','.js':'text/javascript','.mjs':'text/javascript','.css':'text/css','.png':'image/png'})[extname(path)]||'application/octet-stream');res.end(bytes);}catch{res.statusCode=404;res.end();}});
await new Promise(done=>server.listen(0,'127.0.0.1',done));
const browser=await chromium.launch({headless:true,...(process.env.DGXBRIDGE_TEST_BROWSER?{executablePath:process.env.DGXBRIDGE_TEST_BROWSER}:{})});
const page=await browser.newPage({viewport:{width:1140,height:790},reducedMotion:'reduce'});
const errors=[];page.on('pageerror',error=>errors.push(error.message));
await page.addInitScript(()=>{
  const config={ssh_alias:'fixture-spark',hosts:['192.0.2.10'],host_key_alias:'fixture-pinned',user:'example',identity_file:'C:/keys/fixture',port:22,local_build_gate:'C:/buildlock.py',connection_mode:'both',connection_enabled:true,allow_remote:true};
  let limits={cpu_per_job:4,memory_gb_per_job:8,cpu_total:16,memory_gb_total:64,client_jobs:2,client_cpu:8,headroom_gb:32},laptop={concurrency_limit:2,logical_parallelism:12,cpu_memory_caps_enforced:false},update={ok:true,status:'idle',current_version:'0.2.0'};
  window.fixtureCalls=[];
  const device=()=>({ok:true,devices:[{id:'laptop',jobs:[]},{id:'spark',reachable:config.connection_enabled,transport:{authenticated:config.connection_enabled,route:config.connection_enabled?(config.connection_mode==='remote-only'?'paired_iroh':'lan_ssh'):'none'},scheduler:{healthy:config.connection_enabled},...(config.connection_enabled?{}:{error:{message:'Connection is disabled'}})}],shared_board:Array.from({length:37},(_,n)=>({job:`fixture-${n}`,client:n%2?'claude':'codex',where:n%3?'spark':'laptop',state:n<2?'running':'succeeded',exit_code:n<2?null:0,created_at:100-n}))});
  window.__TAURI__={core:{invoke:async(name,args={})=>{
    window.fixtureCalls.push({name,args});
    if(name==='preferences')return {ok:true,config:structuredClone(config),cli_path:'C:/Program Files/DGXBridge/bin/sparkdev.exe',agent_cli_path:'//localhost/C$/Program Files/DGXBridge/bin/sparkdev.exe',agent_state_root:'\\\\localhost\\C$\\Users\\Example\\AppData\\Local\\SparkDevBridge',laptop,connection:{mode:config.connection_mode,enabled:config.connection_enabled},app_version:'0.2.0'};
    if(name==='cli'){if(args.args[0]==='devices')return device();if(args.args[0]==='doctor')return {ok:true,checks:{docker:true},scheduler:{healthy:true}};if(args.args[0]==='setup')return {ok:true};if(args.args[0]==='logs')return {ok:true,text:'Fixture logs'};}
    if(name==='connection'){if(args.mode)config.connection_mode=args.mode;if(typeof args.enabled==='boolean')config.connection_enabled=args.enabled;if(args.laptopJobs)laptop.concurrency_limit=args.laptopJobs;return {ok:true,connection:{mode:config.connection_mode,enabled:config.connection_enabled},laptop};}
    if(name==='resources'){if(args.policy)limits=args.policy;return {ok:true,limits,hardware:{cpu_count:20,memory_gb:120},policy_saved:!!args.policy};}
    if(name==='link')return {ok:true,linked:true,reachable:null,probed:false};
    if(name==='update_action'){if(args.action==='check'){update={...update,busy:true};setTimeout(()=>{update={...update,busy:false,status:'available',available_version:'0.2.1'}},30);return {...update};}if(args.action==='authorize')update={...update,status:'awaiting_approval',verification_code:'FIXTURE',message:'Synthetic approval fixture'};if(args.action==='poll')update={...update,status:'approved'};if(args.action==='download')update={...update,status:'ready'};if(args.action==='install')throw new Error('Installer is deliberately absent from this fixture.');return update;}
    throw new Error(`Unexpected fixture call: ${name}`);
  }}};
});
try{
  await page.goto(`http://127.0.0.1:${server.address().port}/`);await page.getByText('Page 1 of 7',{exact:true}).waitFor();
  assert.equal(await page.locator('#jobs tr').count(),6);assert.equal(await page.locator('[data-connection-state]').first().textContent(),'Connected');
  await page.emulateMedia({reducedMotion:'no-preference'});await page.waitForFunction(()=>document.querySelector('#overview-visual').dataset.motion==='live');assert.ok(await page.locator('#overview-visual canvas').evaluate(el=>el.width)>0);await page.emulateMedia({reducedMotion:'reduce'});await page.waitForFunction(()=>document.querySelector('#overview-visual canvas').width===0);
  await page.locator('#next-page').click();assert.equal(await page.locator('#page-position').textContent(),'Page 2 of 7');
  await page.locator('.nav[data-view="connection"]').click();await page.getByText('This laptop is paired. Its saved identity is preserved.').waitFor();assert.equal(await page.locator('#pair-art').isVisible(),false);await page.locator('#ssh-settings').evaluate(el=>el.open=true);
  await page.locator('#ssh-form button[type=submit]').click();await page.getByText('Connection saved. Choose Test connection when ready; Spark services were not restarted.').waitFor();
  assert.equal(await page.evaluate(()=>fixtureCalls.filter(c=>c.name==='bootstrap').length),0);
  assert.equal(await page.evaluate(()=>fixtureCalls.filter(c=>c.name==='connection'&&c.args.enabled===true).length),0);
  await page.locator('#connection-toggle').click();await page.getByText('Disconnected. Admitted jobs keep running.').waitFor();
  assert.equal(await page.locator('#connection-toggle').textContent(),'Connect');assert.equal(await page.locator('#connection-visual').getAttribute('data-motion'),'static');
  await page.locator('#connection-toggle').click();await page.getByText('Saved connection checked.').waitFor();
  await page.locator('input[name=mode][value="remote-only"]').check();await page.locator('#mode-form button').click();await page.getByText('Connection mode saved. Your pairing is unchanged.').waitFor();
  await page.locator('.nav[data-view="resources"]').click();await page.locator('#resource-fields:not([disabled])').waitFor();await page.locator('input[name=cpu_per_job]').fill('3');await page.locator('#save-resources').click();await page.getByText('Spark limits saved and read back. Running jobs retain their admitted limits.').waitFor();
  await page.locator('.nav[data-view="agents"]').click();await page.locator('[data-agent=claude]').click();assert.match(await page.locator('#agent-prompt').textContent(),/client label claude/);assert.match(await page.locator('#agent-config').textContent(),/SPARKDEV_HOME/);
  await page.locator('#open-updates').click();await page.locator('#updates-dialog').waitFor();await page.locator('[data-update-action=check]').click();await page.locator('[data-update-action=authorize]:visible').waitFor();await page.locator('[data-update-action=authorize]').click();await page.getByText('FIXTURE',{exact:true}).waitFor();
  assert.equal(await page.locator('[data-update-action=download]').isVisible(),false);await page.locator('[data-update-action=download]:visible').waitFor();await page.locator('[data-update-action=download]').click();await page.locator('[data-update-action=install]:visible').waitFor();assert.equal(await page.evaluate(()=>fixtureCalls.filter(c=>c.name==='update_action'&&c.args.action==='install').length),0);await page.locator('#close-updates').click();
  await mkdir('.local',{recursive:true});
  const resourceFooters=[];
  for(const width of [1140,850]){
    await page.setViewportSize({width,height:790});
    for(const view of ['overview','connection','resources','agents']){
      await page.locator(`.nav[data-view="${view}"]`).click();await page.waitForTimeout(80);
      const overflow=await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth);assert.equal(overflow,false,`${view} overflows at ${width}px`);
      if(view==='resources'){
        const footer=await page.locator('#resources-form .form-footer').evaluate(el=>{
          const rect=node=>{const r=node.getBoundingClientRect();return {left:r.left,right:r.right,top:r.top,bottom:r.bottom,width:r.width,height:r.height}};
          const text=el.querySelector('#resource-hardware');
          return {container:rect(el),text:rect(text),button:rect(el.querySelector('#save-resources')),lineHeight:parseFloat(getComputedStyle(text).lineHeight)};
        });
        assert.ok(footer.text.width>=180,`Resources hardware text is compressed at ${width}px: ${JSON.stringify(footer)}`);
        assert.ok(footer.text.height<=footer.lineHeight*3,`Resources hardware text needs more than three lines at ${width}px`);
        const separated=footer.text.right<=footer.button.left||footer.button.right<=footer.text.left||footer.text.bottom<=footer.button.top||footer.button.bottom<=footer.text.top;
        assert.ok(separated,`Resources hardware text overlaps Save at ${width}px`);
        assert.ok(footer.button.left>=footer.container.left&&footer.button.right<=footer.container.right+1,`Resources Save escapes its footer at ${width}px`);
        resourceFooters.push({viewport:width,textWidth:footer.text.width,textHeight:footer.text.height,buttonWidth:footer.button.width});
        await page.screenshot({path:`.local/ui-020-resources-${width}.png`,fullPage:true});
      }
    }
    await page.locator('.nav[data-view=connection]').click();await page.screenshot({path:`.local/ui-020-connection-${width}.png`,fullPage:true});
  }
  assert.deepEqual(errors,[]);console.log(JSON.stringify({ok:true,scope:'isolated browser fixture, not native acceptance',viewports:[1140,850],resourceFooters,checks:['pagination','saved SSH does not bootstrap/connect','disconnect/connect','mode persistence','resource edit/readback','agent instructions and state override','drawer update approval actions','resource footer readable text and non-overlapping Save','no horizontal overflow','no renderer exceptions']}));
}finally{await browser.close();await new Promise(done=>server.close(done));}

/** Stop scheduling graphics frames outside the viewport or while the page is hidden. */
export function startVisibleRenderLoop(host,draw,{
  document=globalThis.document,requestFrame=globalThis.requestAnimationFrame,
  cancelFrame=globalThis.cancelAnimationFrame,IntersectionObserver=globalThis.IntersectionObserver,
}={}){
  let disposed=false,frame=null,inViewport=!IntersectionObserver;
  const visible=()=>!disposed&&!document?.hidden&&inViewport;
  const schedule=()=>{if(visible()&&frame===null)frame=requestFrame(tick);};
  function tick(time){frame=null;if(!visible())return;draw(time);schedule();}
  function changed(){
    if(visible())schedule();
    else if(frame!==null){cancelFrame(frame);frame=null;}
  }
  const observer=IntersectionObserver?new IntersectionObserver(entries=>{
    const entry=entries.find(value=>value.target===host);if(!entry||disposed)return;
    inViewport=entry.isIntersecting===true;changed();
  }):null;
  observer?.observe(host);document?.addEventListener('visibilitychange',changed);schedule();
  return {visible,dispose(){
    if(disposed)return;disposed=true;
    if(frame!==null){cancelFrame(frame);frame=null;}
    observer?.disconnect();document?.removeEventListener('visibilitychange',changed);
  }};
}

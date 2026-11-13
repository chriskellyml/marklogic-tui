import { useRef } from 'react';
export function SidebarResizeHandle({width,onChange}:{width:number;onChange:(width:number)=>void}) {
 const drag=useRef<{x:number;width:number}|null>(null);
 return <div className="sidebar-resizer" role="separator" aria-label="Resize file sidebar" aria-orientation="vertical" aria-valuemin={240} aria-valuemax={900} aria-valuenow={width} tabIndex={0}
   onPointerDown={(e)=>{if(e.button!==0)return;e.preventDefault();drag.current={x:e.clientX,width};e.currentTarget.setPointerCapture(e.pointerId);}}
   onPointerMove={(e)=>{if(drag.current)onChange(drag.current.width+e.clientX-drag.current.x);}}
   onPointerUp={()=>{drag.current=null;}} onPointerCancel={()=>{drag.current=null;}}
   onKeyDown={(e)=>{if(e.key==='ArrowLeft'||e.key==='ArrowRight'||e.key==='Home'){e.preventDefault();onChange(e.key==='Home'?450:width+(e.key==='ArrowLeft'?-20:20));}}}/>
}

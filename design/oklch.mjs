const f=(x)=>x<=0.0031308?12.92*x:1.055*Math.pow(x,1/2.4)-0.055;
function hex(L,C,H){
  const h=H*Math.PI/180,a=C*Math.cos(h),b=C*Math.sin(h);
  const l=(L+0.3963377774*a+0.2158037573*b)**3,m=(L-0.1055613458*a-0.0638541728*b)**3,s=(L-0.0894841775*a-1.2914855480*b)**3;
  const r=4.0767416621*l-3.3077115913*m+0.2309699292*s,g=-1.2684380046*l+2.6097574011*m-0.3413193965*s,bl=-0.0041960863*l-0.7034186147*m+1.7076147010*s;
  const c=(v)=>Math.round(Math.min(1,Math.max(0,f(v)))*255).toString(16).padStart(2,'0');
  return '#'+c(r)+c(g)+c(bl);
}
const T={
 'paper':        [0.982,0.014,92],
 'paper-alt':    [0.966,0.024,92],
 'doc-ink':      [0.250,0.008,70],
 'chrome':       [0.958,0.010,295],
 'chrome-raised':[0.928,0.015,295],
 'chrome-sunken':[0.885,0.022,295],
 'rule':         [0.872,0.018,295],
 'rule-strong':  [0.775,0.024,295],
 'ink':          [0.280,0.022,295],
 'ink-strong':   [0.200,0.028,295],
 'ink-muted':    [0.520,0.020,295],
 'ink-faint':    [0.680,0.016,295],
 'accent':       [0.500,0.150,295],
 'accent-strong':[0.420,0.170,295],
 'accent-soft':  [0.880,0.062,295],
 'accent-faint': [0.960,0.024,295],
 'alert':        [0.520,0.130,30],
 'alert-soft':   [0.900,0.045,30],
};
for(const[k,v]of Object.entries(T))console.log(k.padEnd(14),hex(...v),' oklch('+v[0]+' '+v[1]+' '+v[2]+')');

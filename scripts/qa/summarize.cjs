const fs = require('fs');
const path = require('path');
const q = (v, n) => { v=[...v].sort((a,b)=>a-b);return v.length ? v[Math.round((v.length-1)*n)] : null; };
for (const label of process.argv.slice(2)) {
 const lines=fs.readFileSync(path.join(__dirname,label+'-pad.csv'),'utf8').split(/\r?\n/);
 const pad=lines.filter(s=>/^\d+,(down|up),/.test(s)).map(s=>s.split(','));
 const frames=lines.filter(s=>s.startsWith('frame,')).map(s=>s.split(','));
 const stats=v=>({n:v.length,median:q(v,.5),p95:q(v,.95),p99:q(v,.99),max:q(v,1)});
 const result={label,input_ms:stats(pad.map(v=>+v[2])),button_to_pixel_ms:stats(pad.map(v=>+v[3])),frame_interval_ms:stats(frames.map(v=>+v[2])),frame_to_pixel_ms:stats(frames.map(v=>+v[3])),gaps_over_50ms:frames.filter(v=>+v[2]>50).length,gaps_over_100ms:frames.filter(v=>+v[2]>100).length,summary:lines.find(s=>s.startsWith('frame_summary,'))};
 console.log(JSON.stringify(result,null,2));
 fs.writeFileSync(path.join(__dirname,label+'-summary.json'),JSON.stringify(result,null,2)+'\n');
}

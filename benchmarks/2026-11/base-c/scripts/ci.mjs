import { readFileSync, readdirSync } from 'node:fs';
let bad = [];
for (const f of readdirSync('src')) { const s = readFileSync('src/' + f, 'utf8');
  if (!s.includes('not implemented') ) {
    if (/console\.log/.test(s)) bad.push(f + ': console.log');
    if (/export (async )?function/.test(s) && !(/\/\*\*[\s\S]*@param[\s\S]*@returns[\s\S]*\*\/\s*export/.test(s))) bad.push(f + ': exported function needs JSDoc with @param and @returns');
    if (/throw new (?!RangeError)/.test(s)) bad.push(f + ': throw RangeError only');
  } }
if (bad.length) { console.error(bad.join('\n')); process.exit(1); } console.log('ci ok');

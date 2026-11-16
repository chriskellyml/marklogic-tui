export type ResultFormat = 'plain'|'json'|'xml'|'html'|'csv'|'psv'|'tsv'|'uris';

export function parseDelimitedRow(line:string, delimiter:string):string[] {
  const fields:string[]=[]; let field='', quoted=false;
  for(let i=0;i<line.length;i++){
    const char=line[i];
    if(char==='"') { if(quoted && line[i+1]==='"'){field+='"';i++;} else quoted=!quoted; }
    else if(char===delimiter&&!quoted){fields.push(field.trim());field='';}
    else field+=char;
  }
  fields.push(field.trim());return fields;
}
export function delimitedRows(items:string[],delimiter:string):string[][] {
  return items.flatMap(item=>item.split(/\r?\n/)).filter(line=>line.length>0).map(line=>parseDelimitedRow(line,delimiter));
}
export function prettyResults(items:string[]):string {
  try { const values=items.map(item=>JSON.parse(item));return JSON.stringify(values.length===1?values[0]:values,null,2); }
  catch { return items.map(item=>{try{return JSON.stringify(JSON.parse(item),null,2);}catch{return item;}}).join('\n'); }
}

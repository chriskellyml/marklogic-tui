export interface CorbiLogRecord { time: string; level: string; message: string }

export function corbiLogRecords(content: string): CorbiLogRecord[] {
  const records: CorbiLogRecord[] = [];
  let time = '';
  for (const line of content.split(/\r?\n/)) {
    const header = line.match(/^(.+?\d{1,2}:\d{2}:\d{2}(?:\s*[AP]M)?)\s+([\w$]+\.)+[\w$]+\s+[\w$<>]+\s*$/);
    if (header) {
      time = header[1].match(/\d{1,2}:\d{2}:\d{2}(?:\s*[AP]M)?/)?.[0] ?? header[1];
      continue;
    }
    const entry = line.match(/^(INFO|WARNING|WARN|SEVERE|ERROR|FATAL|DEBUG|FINE|FINER|FINEST|CONFIG):\s?(.*)$/);
    const message = (entry?.[2] ?? line).replace(/\bcom\.marklogic\.developer\.corb\./g, '');
    if (entry) records.push({ time, level: entry[1], message });
    else if (records.length && /^\s|^Caused by:|^\.{3}/.test(line)) records[records.length - 1].message += '\n' + message;
    else if (line) records.push({ time: '', level: '', message });
  }
  return records;
}

export function logTone(record: CorbiLogRecord) {
  if (/SEVERE|ERROR|FATAL/.test(record.level) || /\b(error|severe|fatal|failed|failure)\b/i.test(record.message)) return 'error';
  if (/WARN/.test(record.level) || /\b(warn|warning)\b/i.test(record.message)) return 'warning';
  if (/\b(success|successful|completed|exiting with code 0)\b/i.test(record.message)) return 'success';
  return 'info';
}

export function reportPreview(content: string) {
  const match = content.match(/^\[Showing (?:the )?(?:first|last) [^\n]+\]\n/);
  return { notice: match?.[0].trim() ?? '', text: match ? content.slice(match[0].length) : content };
}

// Reports can contain quoted delimiters, escaped quotes and multiline fields.
export function reportRows(text: string, delimiter: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = '';
  let quoted = false;
  for (let i = 0; i < text.length; i++) {
    const char = text[i];
    if (char === '"') {
      if (quoted && text[i + 1] === '"') { field += '"'; i++; }
      else if (quoted || !field) quoted = !quoted;
      else field += char;
    } else if (char === delimiter && !quoted) { row.push(field); field = ''; }
    else if ((char === '\n' || char === '\r') && !quoted) {
      if (char === '\r' && text[i + 1] === '\n') i++;
      row.push(field);
      if (row.some(cell => cell.length)) rows.push(row);
      row = []; field = '';
    } else field += char;
  }
  row.push(field);
  if (row.some(cell => cell.length)) rows.push(row);
  return rows;
}

export function reportDelimiter(text: string) {
  const candidates = ['|', '\t', ',', ';'];
  return candidates.find(delimiter => {
    const rows = reportRows(text, delimiter).slice(0, 8);
    return rows.length > 0 && rows[0].length > 1 && rows.every(row => row.length === rows[0].length);
  }) ?? '|';
}

/** Indent JSON without parsing and reserializing its numbers or string escapes. */
export function formatDocumentJson(source: string): string {
  try { JSON.parse(source); } catch { return source; }
  let result = '', depth = 0, quoted = false, escaped = false, previous = '';
  const newline = () => `\n${'  '.repeat(depth)}`;
  for (let i = 0; i < source.length; i++) {
    const char = source[i];
    if (quoted) {
      result += char;
      if (escaped) escaped = false;
      else if (char === '\\') escaped = true;
      else if (char === '"') { quoted = false; previous = char; }
    } else if (char === '"') {
      quoted = true; result += char;
    } else if (/\s/.test(char)) {
      continue;
    } else if (char === '{' || char === '[') {
      result += char; depth++;
      let nextIndex = i + 1;
      while (nextIndex < source.length && /\s/.test(source[nextIndex])) nextIndex++;
      const next = source[nextIndex];
      if (next !== '}' && next !== ']') result += newline();
    } else if (char === '}' || char === ']') {
      depth--;
      if (previous !== '{' && previous !== '[') result += newline();
      result += char;
    } else if (char === ',') result += char + newline();
    else if (char === ':') result += ': ';
    else result += char;
    if (!quoted) previous = char;
  }
  return result;
}

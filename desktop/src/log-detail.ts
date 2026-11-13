/** Format a complete JSON or XML payload after the error-log timestamp and level.
 * Invalid or mixed-content XML is left untouched rather than changing its meaning. */
export function formatLogDetail(raw: string): string {
  const header = raw.match(/^(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?\s+\w+[:+]*\s*)([\s\S]*)$/);
  const prefix = header?.[1] ?? '';
  const payload = header?.[2] ?? raw;
  const value = payload.trim();
  if (value.startsWith('{') || value.startsWith('[')) {
    try { return prefix + JSON.stringify(JSON.parse(value), null, 2); } catch { return raw; }
  }
  if (!value.startsWith('<')) return raw;
  try {
    const document = new DOMParser().parseFromString(value, 'application/xml');
    if (document.querySelector('parsererror') || document.doctype) return raw;
    const serializer = new XMLSerializer();
    function format(node: Node, depth: number): string {
      if (node.nodeType !== Node.ELEMENT_NODE) return serializer.serializeToString(node);
      const element = node as Element;
      const children = [...element.childNodes];
      // Whitespace in mixed content, CDATA, or xml:space can be significant.
      if (element.closest('[xml\\:space="preserve"]') || children.some((child) =>
        child.nodeType === Node.CDATA_SECTION_NODE || (child.nodeType === Node.TEXT_NODE && child.textContent?.trim()))) {
        return serializer.serializeToString(element);
      }
      const meaningful = children.filter((child) => child.nodeType !== Node.TEXT_NODE || child.textContent?.trim());
      if (!meaningful.length) return serializer.serializeToString(element);
      const empty = element.cloneNode(false) as Element;
      const serialized = serializer.serializeToString(empty);
      const open = serialized.replace(/\s*\/>$/, '>');
      if (open === serialized) return serializer.serializeToString(element);
      const indent = '  '.repeat(depth);
      return `${open}\n${meaningful.map((child) => `${indent}  ${format(child, depth + 1)}`).join('\n')}\n${indent}</${element.tagName}>`;
    }
    const nodes = [...document.childNodes].filter((node) => node.nodeType !== Node.TEXT_NODE || node.textContent?.trim());
    const declaration = value.match(/^<\?xml\s[^?]*\?>/)?.[0];
    return prefix + (declaration ? `${declaration}\n` : '') + nodes.map((node) => format(node, 0)).join('\n');
  } catch { return raw; }
}

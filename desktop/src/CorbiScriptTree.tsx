import { useMemo, useState } from 'react';
import { ChevronDown, ChevronRight, FileCode2, Folder, Pencil } from 'lucide-react';

interface Node { name: string; path: string; children?: Node[] }
function tree(paths: string[]) {
  const root: Node = { name: '', path: '', children: [] };
  for (const path of paths) {
    let parent = root;
    const parts = path.replace(/^scripts\//, '').split('/');
    parts.forEach((name, i) => {
      const key = 'scripts/' + parts.slice(0, i + 1).join('/');
      let child = parent.children!.find(node => node.path === key);
      if (!child) {
        child = { name, path: key, children: i < parts.length - 1 ? [] : undefined };
        parent.children!.push(child);
      }
      parent = child;
    });
  }
  return root.children!;
}

export function CorbiScriptTree({ paths, selected, disabled, renameDisabled, onSelect, onRename }: {
  paths: string[]; selected: string; disabled: boolean; renameDisabled: boolean;
  onSelect: (path: string) => void; onRename: (path: string) => void;
}) {
  const nodes = useMemo(() => tree(paths), [paths]);
  const [collapsed, setCollapsed] = useState<string[]>([]);
  function render(nodes: Node[], depth: number) {
    return nodes.map(node => {
      const closed = collapsed.includes(node.path);
      return <div key={node.path}>
        <div className="corbi-tree-row" style={{ paddingLeft: depth * 14 }}>
          {node.children ? <button className="file-button" aria-expanded={!closed} aria-label={node.name + ' folder'} onClick={() => setCollapsed(old => closed ? old.filter(path => path !== node.path) : [...old, node.path])}>
            {closed ? <ChevronRight size={12}/> : <ChevronDown size={12}/>}<Folder size={14}/><span>{node.name}</span></button>
            : <button className="file-button" title={node.path} aria-pressed={selected === node.path} disabled={disabled} onClick={() => onSelect(node.path)}><FileCode2 size={14}/><span>{node.name}</span></button>}
          <button className="corbi-rename-button" aria-label={`Rename ${node.path}`} title="Rename" disabled={renameDisabled} onClick={() => onRename(node.path)}><Pencil size={12}/></button>
        </div>
        {node.children && !closed && render(node.children, depth + 1)}
      </div>;
    });
  }
  return <div aria-label="Scripts">{render(nodes, 0)}</div>;
}

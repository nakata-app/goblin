import { useState, useCallback, useEffect, useRef } from 'react';

interface CommandPaletteProps {
  onCommand: (cmd: string) => void;
  onClose: () => void;
}

interface CommandOption {
  id: string;
  label: string;
  desc: string;
  shortcut?: string;
  category: string;
}

const COMMANDS: CommandOption[] = [
  { id: 'new', label: 'New Session', desc: 'Start a new conversation, clear history', shortcut: '⌘N', category: 'Session' },
  { id: 'sessions', label: 'Show Sessions', desc: 'List all past sessions', shortcut: '⌘⇧S', category: 'Session' },
  { id: 'cost', label: 'Cost Report', desc: 'Token usage and cost details', category: 'Analysis' },
  { id: 'map', label: 'Project Map', desc: 'View project file structure and architecture', category: 'Analysis' },
  { id: 'clear', label: 'Clear Panel', desc: 'Clear the right output panel', category: 'Panel' },
  { id: 'copy', label: 'Copy All', desc: 'Copy right panel output to clipboard', shortcut: '⌘⇧C', category: 'Panel' },
  { id: 'model-fast', label: 'Model: DeepSeek Flash', desc: 'Switch to fast model (for short tasks)', category: 'Model' },
  { id: 'model-pro', label: 'Model: DeepSeek Pro', desc: 'Switch to powerful model (for analysis & coding)', category: 'Model' },
  { id: 'shortcuts', label: 'Keyboard Shortcuts', desc: 'List of all keyboard shortcuts', shortcut: '⌘/', category: 'Help' },
  { id: 'export', label: 'Export Session', desc: 'Save current session as JSONL', category: 'Session' },
  { id: 'repo-status', label: 'Repo Status', desc: 'Show git status and branch info', category: 'Git' },
  { id: 'repo-log', label: 'Recent Commits', desc: 'Show last 10 commits', category: 'Git' },
  { id: 'premortem', label: 'Premortem Analysis', desc: 'Run risk analysis on a plan/project', category: 'Analysis' },
  { id: 'eisenhower', label: 'Eisenhower Matrix', desc: 'Classify tasks by urgency & importance', category: 'Analysis' },
  { id: 'help', label: 'Goblin Help', desc: 'Usage guide and tips', category: 'Help' },
];

export function CommandPalette({ onCommand, onClose }: CommandPaletteProps) {
  const [query, setQuery] = useState('');
  const [selectedIdx, setSelectedIdx] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  const filtered = query
    ? COMMANDS.filter(
        (c) =>
          c.label.toLowerCase().includes(query.toLowerCase()) ||
          c.desc.toLowerCase().includes(query.toLowerCase()) ||
          c.category.toLowerCase().includes(query.toLowerCase())
      )
    : COMMANDS;

  const grouped = query ? null : groupBy(filtered, 'category');

  useEffect(() => {
    setSelectedIdx(0);
  }, [query]);

  useEffect(() => {
    if (!listRef.current) return;
    const el = listRef.current.querySelector('.cmd-selected') as HTMLElement | null;
    if (el) {
      el.scrollIntoView({ block: 'nearest', behavior: 'auto' });
    }
  }, [selectedIdx]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === 'Escape') {
        onClose();
      } else if (e.key === 'ArrowDown') {
        e.preventDefault();
        setSelectedIdx((i) => (i + 1) % filtered.length);
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        setSelectedIdx((i) => (i - 1 + filtered.length) % filtered.length);
      } else if (e.key === 'Enter' && filtered[selectedIdx]) {
        onCommand(filtered[selectedIdx].id);
        onClose();
      }
    },
    [filtered, selectedIdx, onCommand, onClose]
  );

  return (
    <div className="cmd-overlay" onClick={onClose}>
      <div className="cmd-palette" onClick={(e) => e.stopPropagation()}>
        <div className="cmd-input-row">
          <span className="cmd-prompt">⟩</span>
          <input
            className="cmd-input"
            placeholder="Search commands... (⌘K)"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            autoFocus
          />
        </div>
        <div className="cmd-list" ref={listRef}>
          {filtered.length === 0 && (
            <div className="cmd-empty">No results found</div>
          )}
          {grouped
            ? Object.entries(grouped).map(([category, cmds]) => (
                <div key={category} className="cmd-group">
                  <div className="cmd-group-label">{category}</div>
                  {(cmds as CommandOption[]).map((cmd) => {
                    const i = filtered.indexOf(cmd);
                    return (
                      <div
                        key={cmd.id}
                        className={`cmd-item ${i === selectedIdx ? 'cmd-selected' : ''}`}
                        onClick={() => { onCommand(cmd.id); onClose(); }}
                        onMouseEnter={() => setSelectedIdx(i)}
                      >
                        <div className="cmd-item-left">
                          <div className="cmd-item-label">{cmd.label}</div>
                          <div className="cmd-item-desc">{cmd.desc}</div>
                        </div>
                        <div className="cmd-item-right">
                          {cmd.shortcut && <span className="cmd-shortcut">{cmd.shortcut}</span>}
                        </div>
                      </div>
                    );
                  })}
                </div>
              ))
            : filtered.map((cmd, i) => (
                <div
                  key={cmd.id}
                  className={`cmd-item ${i === selectedIdx ? 'cmd-selected' : ''}`}
                  onClick={() => { onCommand(cmd.id); onClose(); }}
                  onMouseEnter={() => setSelectedIdx(i)}
                >
                  <div className="cmd-item-left">
                    <div className="cmd-item-label">{cmd.label}</div>
                    <div className="cmd-item-desc">{cmd.desc}</div>
                  </div>
                  <div className="cmd-item-right">
                    <span className="cmd-category">{cmd.category}</span>
                    {cmd.shortcut && <span className="cmd-shortcut">{cmd.shortcut}</span>}
                  </div>
                </div>
              ))}
        </div>
        <div className="cmd-footer">
          <span><kbd>↑↓</kbd> navigate</span>
          <span><kbd>Enter</kbd> select</span>
          <span><kbd>Esc</kbd> close</span>
          <span>{filtered.length} commands</span>
        </div>
      </div>
    </div>
  );
}

export function groupBy<T>(arr: T[], key: keyof T): Record<string, T[]> {
  return arr.reduce((acc, item) => {
    const group = String(item[key]);
    acc[group] = acc[group] || [];
    acc[group].push(item);
    return acc;
  }, {} as Record<string, T[]>);
}

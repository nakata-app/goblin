import { useState, useCallback, useEffect } from 'react';
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
  { id: 'new', label: 'Yeni oturum', desc: 'Mevcut konusmayi temizle', shortcut: '⌘N', category: 'Oturum' },
  { id: 'cost', label: 'Maliyet raporu', desc: 'Token ve maliyet detayi', category: 'Analiz' },
  { id: 'sessions', label: 'Oturumlar', desc: 'Gecmis oturumlari listele', shortcut: '⌘⇧S', category: 'Oturum' },
  { id: 'map', label: 'Proje haritasi', desc: 'Proje dosya yapisini goster', category: 'Analiz' },
  { id: 'clear', label: 'Cikti panelini temizle', desc: 'Sag paneli bosalt', category: 'Panel' },
  { id: 'copy', label: 'Ciktiyi kopyala', desc: 'Sag panel icerigini panoya kopyala', category: 'Panel' },
  { id: 'model-fast', label: 'Model: Flash', desc: 'Hizli modele gec', category: 'Model' },
  { id: 'model-pro', label: 'Model: Pro', desc: 'Guclu modele gec', category: 'Model' },
];

export function CommandPalette({ onCommand, onClose }: CommandPaletteProps) {
  const [query, setQuery] = useState('');
  const [selectedIdx, setSelectedIdx] = useState(0);

  const filtered = query
    ? COMMANDS.filter(
        (c) =>
          c.label.toLowerCase().includes(query.toLowerCase()) ||
          c.desc.toLowerCase().includes(query.toLowerCase()) ||
          c.category.toLowerCase().includes(query.toLowerCase())
      )
    : COMMANDS;

  useEffect(() => {
    setSelectedIdx(0);
  }, [query]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === 'Escape') {
        onClose();
      } else if (e.key === 'ArrowDown') {
        e.preventDefault();
        setSelectedIdx((i) => Math.min(i + 1, filtered.length - 1));
      } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        setSelectedIdx((i) => Math.max(i - 1, 0));
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
            placeholder="Komut yazin..."
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            autoFocus
          />
        </div>
        <div className="cmd-list">
          {filtered.length === 0 && (
            <div className="cmd-empty">Sonuc bulunamadi</div>
          )}
          {filtered.map((cmd, i) => (
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
          <span><kbd>↑↓</kbd> dolas</span>
          <span><kbd>Enter</kbd> sec</span>
          <span><kbd>Esc</kbd> kapat</span>
        </div>
      </div>
    </div>
  );
}

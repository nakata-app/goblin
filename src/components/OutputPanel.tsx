import { useRef, useEffect } from 'react';

interface OutputPanelProps {
  content: string;
  onCopy: () => void;
  onClear: () => void;
}

export function OutputPanel({ content, onCopy, onClear }: OutputPanelProps) {
  const outputRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (outputRef.current) {
      outputRef.current.scrollTop = outputRef.current.scrollHeight;
    }
  }, [content]);

  const hasContent = content.trim().length > 0;

  return (
    <div className="right-panel">
      <div className="panel-header">
        <span className="panel-header-title">cikti</span>
        <div className="panel-header-actions">
          <button className="header-btn" onClick={onCopy} disabled={!hasContent}>
            kopyala
          </button>
          <button className="header-btn" onClick={onClear} disabled={!hasContent}>
            temizle
          </button>
        </div>
      </div>
      <div className="right-content" ref={outputRef}>
        {hasContent ? (
          <pre className="output-pre">{content}</pre>
        ) : (
          <div className="output-empty">Tool ciktisi burada gorunecek...</div>
        )}
      </div>
    </div>
  );
}

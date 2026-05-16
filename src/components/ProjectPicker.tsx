import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useProjectStore } from '../stores/projectStore';

interface ProjectInfo {
  name: string;
  path: string;
  branch: string | null;
  last_commit: number;
}

function relativeTime(ts: number): string {
  if (ts === 0) return '';
  const diff = Math.floor(Date.now() / 1000) - ts;
  if (diff < 3600) return `${Math.floor(diff / 60)}dk`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}sa`;
  if (diff < 604800) return `${Math.floor(diff / 86400)}gün`;
  return `${Math.floor(diff / 604800)}hf`;
}

interface Props {
  /** Sadece onboarding içinde gösteriliyorsa true — daha büyük list */
  inline?: boolean;
}

export function ProjectPicker({ inline = false }: Props) {
  const cwd = useProjectStore((s) => s.cwd);
  const setCwd = useProjectStore((s) => s.setCwd);
  const projectsRoot = useProjectStore((s) => s.projectsRoot);
  const setProjectsRoot = useProjectStore((s) => s.setProjectsRoot);

  const [open, setOpen] = useState(inline);
  const [projects, setProjects] = useState<ProjectInfo[]>([]);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async (root?: string) => {
    setLoading(true);
    try {
      const list = await invoke<ProjectInfo[]>('list_projects', { root: root ?? projectsRoot ?? null });
      setProjects(list);
    } catch { /* root bulunamadı */ }
    setLoading(false);
  }, [projectsRoot]);

  useEffect(() => {
    if (open || inline) load();
  }, [open, inline, load]);

  const handleSelect = (p: ProjectInfo) => {
    setCwd(p.path);
    if (!inline) setOpen(false);
  };

  const handleBrowse = async () => {
    try {
      const path = await invoke<string | null>('pick_directory');
      if (path) setCwd(path);
      if (!inline) setOpen(false);
    } catch { /* iptal */ }
  };

  const handleChangeRoot = async () => {
    try {
      const path = await invoke<string | null>('pick_directory');
      if (path) {
        setProjectsRoot(path);
        load(path);
      }
    } catch { /* iptal */ }
  };

  const activeLabel = cwd ? cwd.split('/').pop() : null;

  if (inline) {
    return (
      <div className="pp-inline">
        <div className="pp-root-row">
          <span className="pp-root-label">
            {projectsRoot ?? 'otomatik tarama'}
          </span>
          {projectsRoot && (
            <button className="pp-root-change" onClick={() => { setProjectsRoot(null); load(undefined); }}>sıfırla</button>
          )}
          <button className="pp-root-change" onClick={handleChangeRoot}>klasör seç</button>
          <button className="pp-root-change" onClick={() => load()}>↻</button>
        </div>
        <ProjectList
          projects={projects}
          loading={loading}
          cwd={cwd}
          onSelect={handleSelect}
          onBrowse={handleBrowse}
        />
      </div>
    );
  }

  return (
    <div className="pp-wrap">
      <button
        className={`header-btn project-btn ${cwd ? 'project-btn-active' : ''}`}
        onClick={() => setOpen((v) => !v)}
        title={cwd ?? 'Proje seç'}
      >
        {activeLabel ? `📁 ${activeLabel}` : '📁 proje'}
      </button>

      {open && (
        <div className="pp-dropdown">
          <div className="pp-root-row">
            <span className="pp-root-label">{projectsRoot ?? 'otomatik tarama'}</span>
            {projectsRoot && (
              <button className="pp-root-change" onClick={() => { setProjectsRoot(null); load(undefined); }}>sıfırla</button>
            )}
            <button className="pp-root-change" onClick={handleChangeRoot}>klasör seç</button>
            <button className="pp-root-change" onClick={() => load()}>↻</button>
          </div>
          <ProjectList
            projects={projects}
            loading={loading}
            cwd={cwd}
            onSelect={handleSelect}
            onBrowse={handleBrowse}
          />
        </div>
      )}
    </div>
  );
}

function ProjectList({ projects, loading, cwd, onSelect, onBrowse }: {
  projects: ProjectInfo[];
  loading: boolean;
  cwd: string | null;
  onSelect: (p: ProjectInfo) => void;
  onBrowse: () => void;
}) {
  if (loading) return <div className="pp-empty">Taranıyor...</div>;
  if (projects.length === 0) return (
    <div className="pp-empty">
      Git reposu bulunamadı
      <button className="pp-browse" onClick={onBrowse}>Manuel seç</button>
    </div>
  );

  return (
    <div className="pp-list">
      {projects.map((p) => (
        <button
          key={p.path}
          className={`pp-item ${cwd === p.path ? 'pp-item-active' : ''}`}
          onClick={() => onSelect(p)}
        >
          <span className="pp-item-check">{cwd === p.path ? '✓' : ' '}</span>
          <span className="pp-item-name">{p.name}</span>
          <span className="pp-item-meta">
            {p.branch && <span className="pp-branch">{p.branch}</span>}
            {p.last_commit > 0 && <span className="pp-time">{relativeTime(p.last_commit)}</span>}
          </span>
        </button>
      ))}
      <button className="pp-browse" onClick={onBrowse}>Manuel seç...</button>
    </div>
  );
}

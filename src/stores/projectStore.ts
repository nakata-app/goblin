import { create } from 'zustand';
import { persist } from 'zustand/middleware';

interface ProjectStore {
  cwd: string | null;
  setCwd: (path: string | null) => void;
  projectsRoot: string | null;
  setProjectsRoot: (path: string | null) => void;
}

export const useProjectStore = create<ProjectStore>()(
  persist(
    (set) => ({
      cwd: null,
      setCwd: (path) => set({ cwd: path }),
      projectsRoot: null,
      setProjectsRoot: (path) => set({ projectsRoot: path }),
    }),
    { name: 'goblin-project-cwd' }
  )
);

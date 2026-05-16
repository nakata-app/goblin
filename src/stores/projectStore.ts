import { create } from 'zustand';
import { persist } from 'zustand/middleware';

interface ProjectStore {
  cwd: string | null;
  setCwd: (path: string | null) => void;
}

export const useProjectStore = create<ProjectStore>()(
  persist(
    (set) => ({
      cwd: null,
      setCwd: (path) => set({ cwd: path }),
    }),
    { name: 'goblin-project-cwd' }
  )
);

# Goblin Session State
## 2026-05-11

### Completed this session
- 3-panel layout: left=chat (fixed 320px), center=GoblinLive (flex), right=tabbed utility (resizable 18-50%)
- GoblinLive center panel: large procedural SVG (320x340), orbit rings, floating particles, emotion-driven eyes/eyebrows/mouth/skin, pupil tracking, breathing scale, head tilt, ear wiggle
- RightTabs component: Thinking / Tasks / Output / Help tabs with Zustand state
- Code diff (+/-) rendering in Output tab with color-coded additions/removals
- Queue messaging: user can type & send while model thinking, queued message auto-sends after current response
- System prompt: no em dash, no unnecessary **bold
- Hold-drag copy: auto clipboard copy on text selection in chat messages
- Like/dislike buttons removed (ChatPanel + App.tsx)
- Attach button SVG centered properly
- chatStore extended: activeTab, thinkingContent, tasks[], diffContent, upsertTask

### Architecture notes
- GoblinCharacter.tsx (72px strip) replaced by GoblinLive.tsx (full center panel)
- OutputPanel.tsx replaced by RightTabs.tsx (tabbed: thinking/tasks/output/help)
- useAgent.ts now uses sendingRef + queueRef for non-blocking message queue
- panel-chat: fixed 320px, panel-center: flex 1, panel-right: resizable

### How to run
```
cd ~/Projects/goblin && npm run tauri dev
```

import { useMemo } from 'react';
import { useChatStore } from '../stores/chatStore';
import { useAgentStore } from '../stores/agentStore';
import type { RightTab } from '../stores/chatStore';

const TABS: { key: RightTab; label: string }[] = [
  { key: 'dashboard', label: 'Dashboard' },
  { key: 'thinking', label: 'Thinking' },
  { key: 'behavior', label: 'Behavior' },
  { key: 'tasks', label: 'Tasks' },
  { key: 'output', label: 'Output' },
  { key: 'help', label: 'Help' },
];

function renderMarkdown(text: string): string {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/^### (.+)$/gm, '<h3 class="md-heading h3">$1</h3>')
    .replace(/^## (.+)$/gm, '<h2 class="md-heading h2">$1</h2>')
    .replace(/^# (.+)$/gm, '<h1 class="md-heading h1">$1</h1>')
    .replace(/`([^`]+)`/g, '<code class="inline-code">$1</code>')
    .replace(/\*\*\*(.+?)\*\*\*/g, '<strong><em>$1</em></strong>')
    .replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>')
    .replace(/\*(.+?)\*/g, '<em>$1</em>')
    .replace(/^> (.+)$/gm, '<div class="md-quote">$1</div>')
    .replace(/^- (.+)$/gm, '<div class="md-bullet">• $1</div>')
    .replace(/^\d+\. (.+)$/gm, '<div class="md-number">$1</div>')
    .replace(/^---$/gm, '<hr class="md-hr">')
    .replace(/\[(.+?)\]\((.+?)\)/g, '<a class="md-link" href="$2">$1</a>')
    .replace(/\n\n/g, '<br/><br/>')
    .replace(/\n/g, '<br/>');
}

function renderDiff(text: string) {
  if (!text) return null;
  const lines = text.split('\n');
  return (
    <div>
      {lines.map((line, i) => {
        let cls = 'diff-line';
        if (line.startsWith('+') && !line.startsWith('+++')) cls += ' diff-add';
        else if (line.startsWith('-') && !line.startsWith('---')) cls += ' diff-remove';
        else if (line.startsWith('@@')) cls += ' diff-hunk';
        else if (line.startsWith('diff ') || line.startsWith('index ') || line.startsWith('--- ') || line.startsWith('+++ ')) cls += ' diff-header';
        return <div key={i} className={cls}>{line || ' '}</div>;
      })}
    </div>
  );
}

export function RightTabs() {
  const activeTab = useChatStore((s) => s.activeTab);
  const setActiveTab = useChatStore((s) => s.setActiveTab);
  const thinkingContent = useChatStore((s) => s.thinkingContent);
  const tasks = useChatStore((s) => s.tasks);
  const rightPanelContent = useChatStore((s) => s.rightPanelContent);
  const diffContent = useChatStore((s) => s.diffContent);
  const decisions = useChatStore((s) => s.decisions);

  const goblinState = useAgentStore((s) => s.goblinState);
  const model = useAgentStore((s) => s.model);
  const cost = useAgentStore((s) => s.cost);
  const turnCount = useAgentStore((s) => s.turnCount);
  const tokensIn = useAgentStore((s) => s.tokensIn);
  const tokensOut = useAgentStore((s) => s.tokensOut);
  const activeTool = useAgentStore((s) => s.activeTool);
  const error = useAgentStore((s) => s.error);

  const renderedOutput = useMemo(() => {
    if (!rightPanelContent) return null;
    return renderMarkdown(rightPanelContent);
  }, [rightPanelContent]);

  return (
    <div className="right-tabs">
      <div className="tab-bar">
        {TABS.map((tab) => (
          <button
            key={tab.key}
            className={`tab-btn${activeTab === tab.key ? ' tab-active' : ''}`}
            onClick={() => setActiveTab(tab.key)}
          >
            {tab.label}
          </button>
        ))}
      </div>

      <div className="tab-content">
        {activeTab === 'thinking' && (
          thinkingContent ? (
            <div className="output-rendered" dangerouslySetInnerHTML={{ __html: renderMarkdown(thinkingContent) }} />
          ) : (
            <div className="tab-content-empty">No reasoning yet. Send a message to see the model's thinking.</div>
          )
        )}

        {activeTab === 'behavior' && (
          decisions.length === 0 ? (
            <div className="tab-content-empty">
              <div className="output-empty-icon">🧠</div>
              <div>No decisions yet</div>
              <div className="output-empty-hint">Model decisions & tool choices appear here</div>
            </div>
          ) : (
            <div className="behavior-timeline">
              {decisions.map((d) => (
                <div key={d.round} className="decision-card">
                  <div className="decision-header">
                    <span className="decision-round">Round {d.round}</span>
                    <span className={`decision-badge ${d.tools_chosen.length > 0 ? 'decision-tools' : 'decision-response'}`}>
                      {d.tools_chosen.length > 0 ? `${d.tools_chosen.length} tools` : 'response'}
                    </span>
                  </div>
                  <div className="decision-tools-list">
                    {d.tools_chosen.map((t) => (
                      <span key={t} className="decision-tool-tag">{t}</span>
                    ))}
                    {d.tools_chosen.length === 0 && (
                      <span className="decision-tool-tag decision-no-tools">no tools - direct response</span>
                    )}
                  </div>
                  {d.reasoning && (
                    <details className="decision-reasoning">
                      <summary className="decision-reasoning-toggle">reasoning</summary>
                      <div className="decision-reasoning-text">{d.reasoning}</div>
                    </details>
                  )}
                </div>
              ))}
            </div>
          )
        )}

        {activeTab === 'tasks' && (
          tasks.length === 0 ? (
            <div className="tab-content-empty">No active tasks</div>
          ) : (
            <div>
              {tasks.map((t) => (
                <div key={t.id}>
                  <div className="task-item">
                    <span className={`task-status task-status-${t.status}`}>
                      {t.status === 'running' ? '⋯' : t.status === 'done' ? '✓' : t.status === 'error' ? '✗' : '○'}
                    </span>
                    <span className="task-name">{t.name}</span>
                  </div>
                  {t.result && <div className="task-result">{t.result.substring(0, 200)}</div>}
                </div>
              ))}
            </div>
          )
        )}

        {activeTab === 'output' && (
          <>
            {rightPanelContent ? (
              <div className="output-rendered" dangerouslySetInnerHTML={{ __html: renderedOutput ?? '' }} />
            ) : (
              <div className="tab-content-empty">
                <div className="output-empty-icon">✦</div>
                <div>Output will appear here</div>
                <div className="output-empty-hint">Tool results & agent responses</div>
              </div>
            )}
            {diffContent && (
              <div style={{ marginTop: 16 }}>
                <h3 className="md-heading h3">Diff</h3>
                {renderDiff(diffContent)}
              </div>
            )}
          </>
        )}

        {activeTab === 'dashboard' && (
          <div className="dashboard-grid">
            <div className="dash-card">
              <div className="dash-card-label">Agent State</div>
              <div className="dash-card-value">
                <span className={`dash-state-dot dash-state-${goblinState}`} />
                {goblinState}
              </div>
            </div>
            <div className="dash-card">
              <div className="dash-card-label">Model</div>
              <div className="dash-card-value dash-accent">{model}</div>
            </div>
            <div className="dash-card">
              <div className="dash-card-label">Tokens In</div>
              <div className="dash-card-value">{tokensIn.toLocaleString()}</div>
            </div>
            <div className="dash-card">
              <div className="dash-card-label">Tokens Out</div>
              <div className="dash-card-value">{tokensOut.toLocaleString()}</div>
            </div>
            <div className="dash-card">
              <div className="dash-card-label">Total Tokens</div>
              <div className="dash-card-value dash-accent">{(tokensIn + tokensOut).toLocaleString()}</div>
            </div>
            <div className="dash-card">
              <div className="dash-card-label">Cost</div>
              <div className="dash-card-value">${cost.toFixed(4)}</div>
            </div>
            <div className="dash-card">
              <div className="dash-card-label">Turns</div>
              <div className="dash-card-value">{turnCount}</div>
            </div>
            <div className="dash-card">
              <div className="dash-card-label">Active Tool</div>
              <div className="dash-card-value">{activeTool || '—'}</div>
            </div>
            {error && (
              <div className="dash-card dash-card-error">
                <div className="dash-card-label">Error</div>
                <div className="dash-card-value">{error.substring(0, 120)}</div>
              </div>
            )}
            <div className="dash-card dash-card-wide">
              <div className="dash-card-label">Token Efficiency</div>
              <div className="dash-card-value">
                {tokensIn > 0 ? `${((tokensOut / tokensIn) * 100).toFixed(0)}%` : '—'}
                <span className="dash-sub"> out/in ratio</span>
              </div>
            </div>
          </div>
        )}

        {activeTab === 'help' && (
          <div className="output-rendered">
            <h2 className="md-heading h2">Goblin Help</h2>
            <h3 className="md-heading h3">Getting Started</h3>
            <div>Press <kbd>⌘K</kbd> to open command palette</div>
            <div>Press <kbd>⌘N</kbd> to start a new session</div>
            <div>Type your message and press <kbd>Enter</kbd> to send</div>
            <br/>
            <h3 className="md-heading h3">File Tools</h3>
            <div className="md-bullet">read_file, write_file, edit_file, multi_edit</div>
            <h3 className="md-heading h3">Search Tools</h3>
            <div className="md-bullet">grep, glob</div>
            <h3 className="md-heading h3">Shell</h3>
            <div className="md-bullet">bash, bash_background, bash_background_check, bash_background_kill</div>
            <h3 className="md-heading h3">Web & Browser</h3>
            <div className="md-bullet">web_search, web_fetch</div>
            <div className="md-bullet">browser_navigate, browser_click, browser_type, browser_scroll, browser_snapshot, browser_press, browser_vision, browser_console</div>
            <h3 className="md-heading h3">Git</h3>
            <div className="md-bullet">git_status, git_diff, git_commit, git_log, git_pr_create</div>
            <h3 className="md-heading h3">Media</h3>
            <div className="md-bullet">vision_analyze, text_to_speech</div>
            <h3 className="md-heading h3">Meta</h3>
            <div className="md-bullet">delegate_task, premortem, eisenhower</div>
            <h3 className="md-heading h3">Vault (Obsidian)</h3>
            <div className="md-bullet">obsidian_read, obsidian_write, obsidian_search, vault_stats</div>
            <h3 className="md-heading h3">MCP</h3>
            <div className="md-bullet">mcp_connect, mcp_list_tools, mcp_call_tool, mcp_install</div>
            <h3 className="md-heading h3">Skills</h3>
            <div className="md-bullet">skill_list, skill_view, skill_manage</div>
            <h3 className="md-heading h3">Peer</h3>
            <div className="md-bullet">peer_send, peer_broadcast, peer_status, peer_coordinate</div>
          </div>
        )}
      </div>
    </div>
  );
}

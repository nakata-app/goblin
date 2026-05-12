import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';

/* ── Tauri config shapes (matches Rust Config) ── */

interface ConfigData {
  providers: {
    openai: ProviderEntry | null;
    anthropic: ProviderEntry | null;
    nvidia: ProviderEntry | null;
    gemini: ProviderEntryGemini | null;
    glm: ProviderEntryGemini | null;
    generic: GenericProvider[];
    auto_route: AutoRoute;
    multi_agent: MultiAgentSettings;
  };
  agent: AgentSettings;
  tools: ToolsSettings;
  memory: MemorySettings;
  stt: SttSettings;
  tts: TtsSettings;
  mnemonics: MnemonicsSettings;
  mcp: McpSettings;
}

interface MnemonicsSettings {
  enabled: boolean;
  binary: string;
  default_ns: string;
}

interface McpServerEntry {
  command: string;
  args: string[];
  env: Record<string, string>;
  enabled: boolean;
}

interface McpSettings {
  servers: Record<string, McpServerEntry>;
}

interface AgentProfile {
  name: string;
  description: string;
  model?: string;
  provider?: string;
  system_prompt?: string;
  allowed_tools: string[];
  blocked_tools: string[];
  triggers: string[];
  enabled: boolean;
  sandbox: boolean;
}

interface MultiAgentSettings {
  enabled: boolean;
  agents: AgentProfile[];
  max_depth: number;
  max_children: number;
}

interface ProviderEntry {
  api_key: string;
  key_pool: string[];
  base_url: string;
  models: string[];
}

interface ProviderEntryGemini {
  api_key: string;
  key_pool: string[];
  base_url: string;
  models: string[];
}

interface GenericProvider {
  name: string;
  api_key: string;
  key_pool: string[];
  base_url: string;
  models: string[];
  provider_type: string;
}

interface AutoRoute {
  enabled: boolean;
  fast_model: string;
  strong_model: string;
  vision_model: string | null;
}

interface AgentSettings {
  default_model: string;
  max_turns: number;
  max_tokens: number;
  temperature: number;
  context_protect_last_n: number;
  context_hard_limit: number;
  context_target_ratio: number;
}

interface ToolsSettings {
  shell_enabled: boolean;
  browser_enabled: boolean;
  workdir: string | null;
}

interface MemorySettings {
  max_observations: number;
  auto_compact_days: number;
  embedding: EmbeddingSettings;
}

interface EmbeddingSettings {
  enabled: boolean;
  provider: string;
  api_key: string | null;
  base_url: string;
  model: string;
}

interface SttSettings {
  provider: string;
  api_key: string | null;
  base_url: string;
  model: string;
}

interface TtsSettings {
  provider: string;
  api_key: string | null;
  base_url: string;
  model: string;
  voice: string;
}

type TabId = 'providers' | 'agent' | 'multi-agent' | 'memory' | 'tts-stt' | 'mnemonics' | 'mcp' | 'plugins';

interface TestResult {
  success: boolean;
  latencyMs: number;
  statusCode: number;
  endpointReachable: boolean;
  message: string;
}

/* ── Props ── */

interface ConfigPanelProps {
  isOpen: boolean;
  onToggle: () => void;
  onConfigSaved?: () => void;
}

/* ── Helpers ── */

const TAB_LABELS: Record<TabId, string> = {
  providers: 'Providers',
  agent: 'Agent',
  'multi-agent': 'Multi-Agent',
  memory: 'Memory',
  'tts-stt': 'TTS / STT',
  mnemonics: 'Mnemonics',
  mcp: 'MCP Servers',
  plugins: 'Plugins',
};

const PROVIDER_LABELS: Record<string, string> = {
  openai: 'DeepSeek / OpenAI',
  anthropic: 'Anthropic',
  nvidia: 'NVIDIA',
  gemini: 'Google Gemini',
  glm: 'GLM (ZhipuAI)',
};

function defaultConfig(): ConfigData {
  return {
    providers: {
      openai: null,
      anthropic: null,
      nvidia: null,
      gemini: null,
      glm: null,
      generic: [],
      auto_route: { enabled: true, fast_model: 'deepseek-v4-flash', strong_model: 'deepseek-v4-pro', vision_model: null },
      multi_agent: { enabled: false, agents: [], max_depth: 3, max_children: 5 },
    },
    agent: { default_model: 'deepseek-v4-pro', max_turns: 30, max_tokens: 8192, temperature: 0, context_protect_last_n: 20, context_hard_limit: 400, context_target_ratio: 0.8 },
    tools: { shell_enabled: true, browser_enabled: true, workdir: null },
    memory: { max_observations: 5000, auto_compact_days: 30, embedding: { enabled: false, provider: 'openai', api_key: null, base_url: 'https://api.openai.com/v1', model: 'text-embedding-3-small' } },
    stt: { provider: 'none', api_key: null, base_url: 'https://api.openai.com/v1', model: 'whisper-1' },
    tts: { provider: 'macos', api_key: null, base_url: 'https://api.openai.com/v1', model: 'tts-1', voice: 'alloy' },
    mnemonics: { enabled: true, binary: 'mnemonics', default_ns: 'proj:goblin' },
    mcp: { servers: {} },
  };
}

/* ── Component ── */

export function ConfigPanel({ isOpen, onToggle, onConfigSaved }: ConfigPanelProps) {
  const [config, setConfig] = useState<ConfigData>(defaultConfig);
  const [activeTab, setActiveTab] = useState<TabId>('providers');
  const [saving, setSaving] = useState(false);
  const [savedMsg, setSavedMsg] = useState('');
  const [testing, setTesting] = useState<string | null>(null);
  const [testResults, setTestResults] = useState<Record<string, TestResult>>({});
  const [showKeys, setShowKeys] = useState<Record<string, boolean>>({});

  // Load config on open
  useEffect(() => {
    if (!isOpen) return;
    invoke<ConfigData>('get_config')
      .then((cfg) => {
        // Merge with defaults to fill missing fields
        const d = defaultConfig();
        const merged = {
          ...d,
          providers: {
            ...d.providers,
            ...(cfg as unknown as unknown as Record<string, unknown>).providers as object,
            auto_route: {
              ...d.providers.auto_route,
              ...((cfg as unknown as unknown as Record<string, unknown>).providers as unknown as Record<string, unknown>)?.auto_route as object,
            },
          },
          agent: { ...d.agent, ...(cfg as unknown as unknown as Record<string, unknown>).agent as object },
          tools: { ...d.tools, ...(cfg as unknown as unknown as Record<string, unknown>).tools as object },
          memory: {
            ...d.memory,
            ...(cfg as unknown as unknown as Record<string, unknown>).memory as object,
            embedding: {
              ...d.memory.embedding,
              ...(((cfg as unknown as unknown as Record<string, unknown>).memory as unknown as Record<string, unknown>)?.embedding as object),
            },
          },
          stt: { ...d.stt, ...(cfg as unknown as unknown as Record<string, unknown>).stt as object },
          tts: { ...d.tts, ...(cfg as unknown as unknown as Record<string, unknown>).tts as object },
          mnemonics: { ...d.mnemonics, ...(cfg as unknown as unknown as Record<string, unknown>).mnemonics as object },
          mcp: {
            ...d.mcp,
            ...(cfg as unknown as unknown as Record<string, unknown>).mcp as object,
            servers: {
              ...d.mcp.servers,
              ...(((cfg as unknown as unknown as Record<string, unknown>).mcp as unknown as Record<string, unknown>)?.servers as Record<string, McpServerEntry> ?? {}),
            },
          },
        };
        setConfig(merged);
      })
      .catch(() => setConfig(defaultConfig()));
  }, [isOpen]);

  const save = useCallback(async () => {
    setSaving(true);
    setSavedMsg('');
    try {
      await invoke('save_config', { configJson: config });
      setSavedMsg('Saved ✓');
      onConfigSaved?.();
      setTimeout(() => setSavedMsg(''), 2000);
    } catch (e) {
      setSavedMsg(`Error: ${String(e)}`);
    } finally {
      setSaving(false);
    }
  }, [config, onConfigSaved]);

  const testProvider = useCallback(async (providerId: string) => {
    setTesting(providerId);
    let key = '';
    let baseUrl = '';

    if (providerId === 'openai' && config.providers.openai) {
      key = config.providers.openai.api_key;
      baseUrl = config.providers.openai.base_url;
    } else if (providerId === 'anthropic' && config.providers.anthropic) {
      key = config.providers.anthropic.api_key;
      baseUrl = config.providers.anthropic.base_url;
    } else if (providerId === 'nvidia' && config.providers.nvidia) {
      key = config.providers.nvidia.api_key;
      baseUrl = config.providers.nvidia.base_url;
    } else if (providerId === 'gemini' && config.providers.gemini) {
      key = config.providers.gemini.api_key;
      baseUrl = config.providers.gemini.base_url;
    } else if (providerId === 'glm' && config.providers.glm) {
      key = config.providers.glm.api_key;
      baseUrl = config.providers.glm.base_url;
    }

    if (!key) {
      setTestResults((p) => ({ ...p, [providerId]: { success: false, latencyMs: 0, statusCode: 0, endpointReachable: false, message: 'No API key configured' } }));
      setTesting(null);
      return;
    }

    try {
      const result = await invoke<TestResult>('test_connection', { apiKey: key, baseUrl, providerType: providerId === 'anthropic' ? 'anthropic' : 'openai' });
      setTestResults((p) => ({ ...p, [providerId]: result }));
    } catch (e) {
      setTestResults((p) => ({ ...p, [providerId]: { success: false, latencyMs: 0, statusCode: 0, endpointReachable: false, message: String(e) } }));
    } finally {
      setTesting(null);
    }
  }, [config]);

  const toggleKeyVisibility = (id: string) => {
    setShowKeys((p) => ({ ...p, [id]: !p[id] }));
  };

  if (!isOpen) return null;

  /* ── Provider editing helpers ── */

  const updateProvider = (id: string, field: string, value: unknown) => {
    setConfig((c) => {
      const provs = { ...c.providers };
      if (id in provs) {
        const existing = (provs as unknown as Record<string, unknown>)[id];
        if (existing) {
          (provs as unknown as Record<string, unknown>)[id] = { ...(existing as object), [field]: value };
        }
      }
      return { ...c, providers: provs };
    });
  };

  const ensureProvider = (id: string) => {
    setConfig((c) => {
      const provs = { ...c.providers };
      if (!(provs as unknown as Record<string, unknown>)[id]) {
        const defaults: Record<string, ProviderEntry | ProviderEntryGemini> = {
          openai: { api_key: '', key_pool: [], base_url: 'https://api.deepseek.com/v1', models: ['deepseek-v4-pro', 'deepseek-v4-flash'] },
          anthropic: { api_key: '', key_pool: [], base_url: 'https://api.anthropic.com/v1', models: ['claude-sonnet-4-20250514'] },
          nvidia: { api_key: '', key_pool: [], base_url: 'https://integrate.api.nvidia.com/v1', models: ['nvidia/llama-3.1-nemotron-70b-instruct'] },
          gemini: { api_key: '', key_pool: [], base_url: 'https://generativelanguage.googleapis.com/v1beta', models: ['gemini-2.5-flash'] },
          glm: { api_key: '', key_pool: [], base_url: 'https://open.bigmodel.cn/api/paas/v4', models: ['glm-4-plus'] },
        };
        (provs as unknown as Record<string, unknown>)[id] = defaults[id];
      }
      return { ...c, providers: provs };
    });
  };

  const removeProvider = (id: string) => {
    setConfig((c) => {
      const provs = { ...c.providers };
      (provs as unknown as Record<string, unknown>)[id] = null;
      return { ...c, providers: provs };
    });
  };

  const updateAutoRoute = (field: string, value: unknown) => {
    setConfig((c) => ({
      ...c,
      providers: { ...c.providers, auto_route: { ...c.providers.auto_route, [field]: value } },
    }));
  };

  return (
    <>
      <div className={`config-overlay ${isOpen ? 'config-open' : ''}`} onClick={onToggle} />
      <div className={`config-panel ${isOpen ? 'config-panel-open' : ''}`}>
        <div className="config-header">
          <span className="config-title">Settings</span>
          <button className="config-close" onClick={onToggle}>✕</button>
        </div>

        {/* Tabs */}
        <div className="config-tabs">
          {(Object.keys(TAB_LABELS) as TabId[]).map((t) => (
            <button
              key={t}
              className={`config-tab ${activeTab === t ? 'config-tab-active' : ''}`}
              onClick={() => setActiveTab(t)}
            >
              {TAB_LABELS[t]}
            </button>
          ))}
        </div>

        <div className="config-body">
          {/* ═══ PROVIDERS ═══ */}
          {activeTab === 'providers' && (
            <div className="config-section">
              {/* Auto-Route */}
              <div className="config-subsection">
                <h4 className="config-subsection-title">Auto-Routing</h4>
                <label className="config-row">
                  <span>Enabled</span>
                  <input type="checkbox" checked={config.providers.auto_route.enabled} onChange={(e) => updateAutoRoute('enabled', e.target.checked)} />
                </label>
                <label className="config-row">
                  <span>Fast Model</span>
                  <input className="config-input" value={config.providers.auto_route.fast_model} onChange={(e) => updateAutoRoute('fast_model', e.target.value)} />
                </label>
                <label className="config-row">
                  <span>Strong Model</span>
                  <input className="config-input" value={config.providers.auto_route.strong_model} onChange={(e) => updateAutoRoute('strong_model', e.target.value)} />
                </label>
                <label className="config-row">
                  <span>Vision Model</span>
                  <input className="config-input" value={config.providers.auto_route.vision_model ?? ''} placeholder="(none)" onChange={(e) => updateAutoRoute('vision_model', e.target.value || null)} />
                </label>
              </div>

              {/* Per-provider configs */}
              {(['openai', 'anthropic', 'nvidia', 'gemini', 'glm'] as const).map((id) => {
                const prov = config.providers[id];
                const testResult = testResults[id];
                const isTest = testing === id;

                return (
                  <div key={id} className="config-provider-card">
                    <div className="config-provider-header">
                      <h4 className="config-provider-name">{PROVIDER_LABELS[id]}</h4>
                      {prov ? (
                        <button className="config-btn-sm config-btn-danger" onClick={() => removeProvider(id)}>Remove</button>
                      ) : (
                        <button className="config-btn-sm" onClick={() => ensureProvider(id)}>Configure</button>
                      )}
                    </div>

                    {prov && (
                      <>
                        <label className="config-row">
                          <span>API Key</span>
                          <div className="config-input-group">
                            <input
                              className="config-input"
                              type={showKeys[id] ? 'text' : 'password'}
                              value={prov.api_key}
                              placeholder="sk-..."
                              onChange={(e) => updateProvider(id, 'api_key', e.target.value)}
                            />
                            <button className="config-btn-sm config-btn-ghost" onClick={() => toggleKeyVisibility(id)}>
                              {showKeys[id] ? 'Hide' : 'Show'}
                            </button>
                          </div>
                        </label>
                        <label className="config-row">
                          <span>Base URL</span>
                          <input className="config-input" value={prov.base_url} onChange={(e) => updateProvider(id, 'base_url', e.target.value)} />
                        </label>
                        <label className="config-row">
                          <span>Models (comma separated)</span>
                          <input className="config-input" value={prov.models.join(', ')} onChange={(e) => updateProvider(id, 'models', e.target.value.split(',').map((s) => s.trim()).filter(Boolean))} />
                        </label>
                        <label className="config-row">
                          <span>Key Pool (comma separated)</span>
                          <input className="config-input" value={prov.key_pool.join(', ')} placeholder="optional extra keys" onChange={(e) => updateProvider(id, 'key_pool', e.target.value.split(',').map((s) => s.trim()).filter(Boolean))} />
                        </label>
                        <div className="config-row">
                          <span />
                          <div className="config-actions">
                            <button className="config-btn" disabled={isTest} onClick={() => testProvider(id)}>
                              {isTest ? 'Testing...' : 'Test Connection'}
                            </button>
                            {testResult && (
                              <span className={`config-test-result ${testResult.success ? 'config-test-ok' : 'config-test-fail'}`}>
                                {testResult.success ? `✓ ${testResult.latencyMs}ms` : `✗ ${testResult.message}`}
                              </span>
                            )}
                          </div>
                        </div>
                      </>
                    )}
                  </div>
                );
              })}

              {/* Generic providers */}
              <div className="config-subsection">
                <h4 className="config-subsection-title">
                  Generic Providers
                  <button className="config-btn-sm" onClick={() => {
                    setConfig((c) => ({
                      ...c,
                      providers: {
                        ...c.providers,
                        generic: [...c.providers.generic, { name: '', api_key: '', key_pool: [], base_url: '', models: [], provider_type: 'openai' }],
                      },
                    }));
                  }}>+ Add</button>
                </h4>
                {config.providers.generic.map((g, i) => (
                  <div key={i} className="config-provider-card">
                    <div className="config-provider-header">
                      <input className="config-input" style={{ width: 180 }} value={g.name} placeholder="Provider name (e.g. groq)" onChange={(e) => {
                        const updated = [...config.providers.generic];
                        updated[i] = { ...updated[i], name: e.target.value };
                        setConfig((c) => ({ ...c, providers: { ...c.providers, generic: updated } }));
                      }} />
                      <select className="config-input" style={{ width: 110 }} value={g.provider_type} onChange={(e) => {
                        const updated = [...config.providers.generic];
                        updated[i] = { ...updated[i], provider_type: e.target.value };
                        setConfig((c) => ({ ...c, providers: { ...c.providers, generic: updated } }));
                      }}>
                        <option value="openai">OpenAI</option>
                        <option value="anthropic">Anthropic</option>
                      </select>
                      <button className="config-btn-sm config-btn-danger" onClick={() => {
                        setConfig((c) => ({ ...c, providers: { ...c.providers, generic: c.providers.generic.filter((_, j) => j !== i) } }));
                      }}>✕</button>
                    </div>
                    <label className="config-row">
                      <span>API Key</span>
                      <input className="config-input" value={g.api_key} onChange={(e) => {
                        const updated = [...config.providers.generic];
                        updated[i] = { ...updated[i], api_key: e.target.value };
                        setConfig((c) => ({ ...c, providers: { ...c.providers, generic: updated } }));
                      }} />
                    </label>
                    <label className="config-row">
                      <span>Base URL</span>
                      <input className="config-input" value={g.base_url} onChange={(e) => {
                        const updated = [...config.providers.generic];
                        updated[i] = { ...updated[i], base_url: e.target.value };
                        setConfig((c) => ({ ...c, providers: { ...c.providers, generic: updated } }));
                      }} />
                    </label>
                    <label className="config-row">
                      <span>Models</span>
                      <input className="config-input" value={g.models.join(', ')} onChange={(e) => {
                        const updated = [...config.providers.generic];
                        updated[i] = { ...updated[i], models: e.target.value.split(',').map((s) => s.trim()).filter(Boolean) };
                        setConfig((c) => ({ ...c, providers: { ...c.providers, generic: updated } }));
                      }} />
                    </label>
                  </div>
                ))}
              </div>
            </div>
          )}

          {/* ═══ AGENT ═══ */}
          {activeTab === 'agent' && (
            <div className="config-section">
              <label className="config-row">
                <span>Default Model</span>
                <input className="config-input" value={config.agent.default_model} onChange={(e) => setConfig((c) => ({ ...c, agent: { ...c.agent, default_model: e.target.value } }))} />
              </label>
              <label className="config-row">
                <span>Max Turns</span>
                <input className="config-input" type="number" value={config.agent.max_turns} onChange={(e) => setConfig((c) => ({ ...c, agent: { ...c.agent, max_turns: Number(e.target.value) } }))} />
              </label>
              <label className="config-row">
                <span>Max Tokens</span>
                <input className="config-input" type="number" value={config.agent.max_tokens} onChange={(e) => setConfig((c) => ({ ...c, agent: { ...c.agent, max_tokens: Number(e.target.value) } }))} />
              </label>
              <label className="config-row">
                <span>Temperature</span>
                <input className="config-input" type="number" step="0.1" min="0" max="2" value={config.agent.temperature} onChange={(e) => setConfig((c) => ({ ...c, agent: { ...c.agent, temperature: Number(e.target.value) } }))} />
              </label>
              <h4 className="config-subsection-title">Context Window</h4>
              <label className="config-row">
                <span>Protect Last N Messages</span>
                <input className="config-input" type="number" value={config.agent.context_protect_last_n} onChange={(e) => setConfig((c) => ({ ...c, agent: { ...c.agent, context_protect_last_n: Number(e.target.value) } }))} />
              </label>
              <label className="config-row">
                <span>Hard Limit (messages)</span>
                <input className="config-input" type="number" value={config.agent.context_hard_limit} onChange={(e) => setConfig((c) => ({ ...c, agent: { ...c.agent, context_hard_limit: Number(e.target.value) } }))} />
              </label>
              <label className="config-row">
                <span>Target Compression Ratio</span>
                <input className="config-input" type="number" step="0.05" min="0.1" max="1" value={config.agent.context_target_ratio} onChange={(e) => setConfig((c) => ({ ...c, agent: { ...c.agent, context_target_ratio: Number(e.target.value) } }))} />
              </label>

              <label className="config-row" style={{ marginTop: 12 }}>
                <span>Shell Enabled</span>
                <input type="checkbox" checked={config.tools.shell_enabled} onChange={(e) => setConfig((c) => ({ ...c, tools: { ...c.tools, shell_enabled: e.target.checked } }))} />
              </label>
              <label className="config-row">
                <span>Browser Enabled</span>
                <input type="checkbox" checked={config.tools.browser_enabled} onChange={(e) => setConfig((c) => ({ ...c, tools: { ...c.tools, browser_enabled: e.target.checked } }))} />
              </label>
              <label className="config-row">
                <span>Default Workdir</span>
                <input className="config-input" value={config.tools.workdir ?? ''} placeholder="(current directory)" onChange={(e) => setConfig((c) => ({ ...c, tools: { ...c.tools, workdir: e.target.value || null } }))} />
              </label>
            </div>
          )}

          {/* ═══ MULTI-AGENT ═══ */}
          {activeTab === 'multi-agent' && (
            <div className="config-section">
              <label className="config-row">
                <span>Enable Multi-Agent Routing</span>
                <input type="checkbox" checked={config.providers.multi_agent.enabled} onChange={(e) => setConfig((c) => ({
                  ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, enabled: e.target.checked } }
                }))} />
              </label>
              {config.providers.multi_agent.enabled && (
                <>
                  <label className="config-row">
                    <span>Max Depth</span>
                    <input className="config-input" type="number" min={1} max={5} value={config.providers.multi_agent.max_depth} onChange={(e) => setConfig((c) => ({
                      ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, max_depth: Number(e.target.value) } }
                    }))} />
                  </label>
                  <label className="config-row">
                    <span>Max Children Per Agent</span>
                    <input className="config-input" type="number" min={1} max={10} value={config.providers.multi_agent.max_children} onChange={(e) => setConfig((c) => ({
                      ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, max_children: Number(e.target.value) } }
                    }))} />
                  </label>
                  <h4 className="config-subsection-title" style={{ marginTop: 16 }}>Agent Profiles</h4>
                  {config.providers.multi_agent.agents.map((agent, idx) => (
                    <div key={idx} className="config-agent-card">
                      <div className="config-agent-header">
                        <input className="config-input" style={{ flex: 1 }} value={agent.name} placeholder="Agent name"
                          onChange={(e) => {
                            const agents = [...config.providers.multi_agent.agents];
                            agents[idx] = { ...agents[idx], name: e.target.value };
                            setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                          }} />
                        <label><input type="checkbox" checked={agent.enabled} onChange={(e) => {
                          const agents = [...config.providers.multi_agent.agents];
                          agents[idx] = { ...agents[idx], enabled: e.target.checked };
                          setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                        }} /> Enabled</label>
                        <button className="config-btn config-btn-sm" onClick={() => {
                          const agents = config.providers.multi_agent.agents.filter((_, i) => i !== idx);
                          setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                        }}>✕</button>
                      </div>
                      <label className="config-row"><span>Description</span>
                        <input className="config-input" value={agent.description} placeholder="What this agent does" onChange={(e) => {
                          const agents = [...config.providers.multi_agent.agents];
                          agents[idx] = { ...agents[idx], description: e.target.value };
                          setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                        }} />
                      </label>
                      <label className="config-row"><span>Model (optional)</span>
                        <input className="config-input" value={agent.model ?? ''} placeholder="e.g. deepseek-v4-pro" onChange={(e) => {
                          const agents = [...config.providers.multi_agent.agents];
                          agents[idx] = { ...agents[idx], model: e.target.value || undefined };
                          setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                        }} />
                      </label>
                      <label className="config-row"><span>Triggers (comma-separated)</span>
                        <input className="config-input" value={agent.triggers.join(', ')} placeholder="debug, analyze, review" onChange={(e) => {
                          const agents = [...config.providers.multi_agent.agents];
                          agents[idx] = { ...agents[idx], triggers: e.target.value.split(',').map(s => s.trim()).filter(Boolean) };
                          setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                        }} />
                      </label>
                      <label className="config-row"><span>Allowed Tools (comma-separated, empty=all)</span>
                        <input className="config-input" value={agent.allowed_tools.join(', ')} placeholder="bash, read_file, grep" onChange={(e) => {
                          const agents = [...config.providers.multi_agent.agents];
                          agents[idx] = { ...agents[idx], allowed_tools: e.target.value.split(',').map(s => s.trim()).filter(Boolean) };
                          setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                        }} />
                      </label>
                      <label className="config-row"><span>Sandbox</span>
                        <input type="checkbox" checked={agent.sandbox} onChange={(e) => {
                          const agents = [...config.providers.multi_agent.agents];
                          agents[idx] = { ...agents[idx], sandbox: e.target.checked };
                          setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                        }} />
                      </label>
                    </div>
                  ))}
                  <button className="config-btn" style={{ marginTop: 8 }} onClick={() => {
                    const agents = [...config.providers.multi_agent.agents, {
                      name: '', description: '', model: undefined, provider: undefined,
                      system_prompt: undefined, allowed_tools: [], blocked_tools: [],
                      triggers: [], enabled: true, sandbox: false
                    }];
                    setConfig((c) => ({ ...c, providers: { ...c.providers, multi_agent: { ...c.providers.multi_agent, agents } } }));
                  }}>+ Add Agent</button>
                </>
              )}
            </div>
          )}

          {/* ═══ MEMORY ═══ */}
          {activeTab === 'memory' && (
            <div className="config-section">
              <label className="config-row">
                <span>Max Observations</span>
                <input className="config-input" type="number" value={config.memory.max_observations} onChange={(e) => setConfig((c) => ({ ...c, memory: { ...c.memory, max_observations: Number(e.target.value) } }))} />
              </label>
              <label className="config-row">
                <span>Auto-Compact Days</span>
                <input className="config-input" type="number" value={config.memory.auto_compact_days} onChange={(e) => setConfig((c) => ({ ...c, memory: { ...c.memory, auto_compact_days: Number(e.target.value) } }))} />
              </label>

              <h4 className="config-subsection-title" style={{ marginTop: 16 }}>Embedding / Semantic Search</h4>
              <label className="config-row">
                <span>Enabled</span>
                <input type="checkbox" checked={config.memory.embedding.enabled} onChange={(e) => setConfig((c) => ({ ...c, memory: { ...c.memory, embedding: { ...c.memory.embedding, enabled: e.target.checked } } }))} />
              </label>
              <label className="config-row">
                <span>Provider</span>
                <input className="config-input" value={config.memory.embedding.provider} onChange={(e) => setConfig((c) => ({ ...c, memory: { ...c.memory, embedding: { ...c.memory.embedding, provider: e.target.value } } }))} />
              </label>
              <label className="config-row">
                <span>API Key</span>
                <input className="config-input" type="password" value={config.memory.embedding.api_key ?? ''} placeholder="(uses provider key if empty)" onChange={(e) => setConfig((c) => ({ ...c, memory: { ...c.memory, embedding: { ...c.memory.embedding, api_key: e.target.value || null } } }))} />
              </label>
              <label className="config-row">
                <span>Base URL</span>
                <input className="config-input" value={config.memory.embedding.base_url} onChange={(e) => setConfig((c) => ({ ...c, memory: { ...c.memory, embedding: { ...c.memory.embedding, base_url: e.target.value } } }))} />
              </label>
              <label className="config-row">
                <span>Model</span>
                <input className="config-input" value={config.memory.embedding.model} onChange={(e) => setConfig((c) => ({ ...c, memory: { ...c.memory, embedding: { ...c.memory.embedding, model: e.target.value } } }))} />
              </label>
            </div>
          )}

          {/* ═══ TTS / STT ═══ */}
          {activeTab === 'tts-stt' && (
            <div className="config-section">
              <h4 className="config-subsection-title">Text-to-Speech</h4>
              <label className="config-row">
                <span>Provider</span>
                <select className="config-input" value={config.tts.provider} onChange={(e) => setConfig((c) => ({ ...c, tts: { ...c.tts, provider: e.target.value } }))}>
                  <option value="macos">macOS (built-in)</option>
                  <option value="openai">OpenAI TTS</option>
                  <option value="edge">Microsoft Edge</option>
                </select>
              </label>
              <label className="config-row">
                <span>API Key</span>
                <input className="config-input" type="password" value={config.tts.api_key ?? ''} onChange={(e) => setConfig((c) => ({ ...c, tts: { ...c.tts, api_key: e.target.value || null } }))} />
              </label>
              <label className="config-row">
                <span>Base URL</span>
                <input className="config-input" value={config.tts.base_url} onChange={(e) => setConfig((c) => ({ ...c, tts: { ...c.tts, base_url: e.target.value } }))} />
              </label>
              <label className="config-row">
                <span>Model</span>
                <input className="config-input" value={config.tts.model} onChange={(e) => setConfig((c) => ({ ...c, tts: { ...c.tts, model: e.target.value } }))} />
              </label>
              <label className="config-row">
                <span>Voice</span>
                <input className="config-input" value={config.tts.voice} placeholder="alloy, echo, fable, nova, onyx, shimmer" onChange={(e) => setConfig((c) => ({ ...c, tts: { ...c.tts, voice: e.target.value } }))} />
              </label>

              <h4 className="config-subsection-title" style={{ marginTop: 20 }}>Speech-to-Text</h4>
              <label className="config-row">
                <span>Provider</span>
                <select className="config-input" value={config.stt.provider} onChange={(e) => setConfig((c) => ({ ...c, stt: { ...c.stt, provider: e.target.value } }))}>
                  <option value="none">None</option>
                  <option value="openai">OpenAI Whisper</option>
                </select>
              </label>
              <label className="config-row">
                <span>API Key</span>
                <input className="config-input" type="password" value={config.stt.api_key ?? ''} onChange={(e) => setConfig((c) => ({ ...c, stt: { ...c.stt, api_key: e.target.value || null } }))} />
              </label>
              <label className="config-row">
                <span>Base URL</span>
                <input className="config-input" value={config.stt.base_url} onChange={(e) => setConfig((c) => ({ ...c, stt: { ...c.stt, base_url: e.target.value } }))} />
              </label>
              <label className="config-row">
                <span>Model</span>
                <input className="config-input" value={config.stt.model} onChange={(e) => setConfig((c) => ({ ...c, stt: { ...c.stt, model: e.target.value } }))} />
              </label>
            </div>
          )}

          {/* ═══ Mnemonics ═══ */}
          {activeTab === 'mnemonics' && (
            <div className="config-section">
              <p className="config-section-desc">
                Cross-project semantic memory via Atakan's `mnemonics` binary. Goblin auto-detects
                the binary at boot; if it is missing the agent simply loses the
                <code> mnemonics_retrieve</code> / <code> mnemonics_ingest</code> tools.
              </p>
              <label className="config-row">
                <span>Enabled</span>
                <input type="checkbox" checked={config.mnemonics.enabled} onChange={(e) => setConfig((c) => ({ ...c, mnemonics: { ...c.mnemonics, enabled: e.target.checked } }))} />
              </label>
              <label className="config-row">
                <span>Binary</span>
                <input className="config-input" value={config.mnemonics.binary} placeholder="mnemonics" onChange={(e) => setConfig((c) => ({ ...c, mnemonics: { ...c.mnemonics, binary: e.target.value } }))} />
              </label>
              <label className="config-row">
                <span>Default namespace</span>
                <input className="config-input" value={config.mnemonics.default_ns} placeholder="proj:goblin" onChange={(e) => setConfig((c) => ({ ...c, mnemonics: { ...c.mnemonics, default_ns: e.target.value } }))} />
              </label>
            </div>
          )}

          {/* ═══ MCP Servers ═══ */}
          {activeTab === 'mcp' && (
            <McpServersEditor servers={config.mcp.servers} onChange={(servers) => setConfig((c) => ({ ...c, mcp: { ...c.mcp, servers } }))} />
          )}

          {/* ═══ Plugins ═══ */}
          {activeTab === 'plugins' && (
            <PluginsEditor />
          )}
        </div>

        {/* Footer */}
        <div className="config-footer">
          <button className="config-btn config-btn-primary" disabled={saving} onClick={save}>
            {saving ? 'Saving...' : 'Save Config'}
          </button>
          {savedMsg && <span className={`config-saved ${savedMsg.startsWith('Error') ? 'config-saved-err' : ''}`}>{savedMsg}</span>}
        </div>
      </div>
    </>
  );
}

/* ── MCP servers editor ── */

function McpServersEditor({
  servers,
  onChange,
}: {
  servers: Record<string, McpServerEntry>;
  onChange: (servers: Record<string, McpServerEntry>) => void;
}) {
  const [newName, setNewName] = useState('');

  const addServer = () => {
    const name = newName.trim();
    if (!name || servers[name]) return;
    onChange({
      ...servers,
      [name]: { command: '', args: [], env: {}, enabled: true },
    });
    setNewName('');
  };

  const removeServer = (name: string) => {
    const next = { ...servers };
    delete next[name];
    onChange(next);
  };

  const updateServer = (name: string, patch: Partial<McpServerEntry>) => {
    onChange({ ...servers, [name]: { ...servers[name], ...patch } });
  };

  const entries = Object.entries(servers);

  return (
    <div className="config-section">
      <p className="config-section-desc">
        MCP servers auto-boot on launch and expose their tools to the agent via
        <code> mcp_servers</code> / <code> mcp_tools</code> / <code> mcp_call</code>. Use the same
        shape as Claude Code: <code>command</code>, <code>args</code>, optional <code>env</code>.
      </p>

      <div className="config-row">
        <input
          className="config-input"
          placeholder="new server name (e.g. github)"
          value={newName}
          onChange={(e) => setNewName(e.target.value)}
          onKeyDown={(e) => { if (e.key === 'Enter') addServer(); }}
        />
        <button className="config-btn config-btn-sm" onClick={addServer}>Add</button>
      </div>

      {entries.length === 0 && <p className="config-section-desc">No MCP servers configured.</p>}

      {entries.map(([name, srv]) => (
        <div key={name} className="config-provider-card">
          <div className="config-provider-header">
            <h4 className="config-provider-name">{name}</h4>
            <div className="config-provider-actions">
              <label className="config-row" style={{ margin: 0 }}>
                <span>Enabled</span>
                <input type="checkbox" checked={srv.enabled} onChange={(e) => updateServer(name, { enabled: e.target.checked })} />
              </label>
              <button className="config-btn-sm config-btn-danger" onClick={() => removeServer(name)}>Remove</button>
            </div>
          </div>
          <label className="config-row">
            <span>Command</span>
            <input className="config-input" placeholder="/path/to/bin or npx" value={srv.command} onChange={(e) => updateServer(name, { command: e.target.value })} />
          </label>
          <label className="config-row">
            <span>Args (one per line)</span>
            <textarea
              className="config-input"
              rows={3}
              value={srv.args.join('\n')}
              onChange={(e) => updateServer(name, { args: e.target.value.split('\n').filter((s) => s.length > 0) })}
            />
          </label>
          <label className="config-row">
            <span>Env (KEY=value per line)</span>
            <textarea
              className="config-input"
              rows={3}
              value={Object.entries(srv.env).map(([k, v]) => `${k}=${v}`).join('\n')}
              onChange={(e) => {
                const next: Record<string, string> = {};
                for (const line of e.target.value.split('\n')) {
                  const idx = line.indexOf('=');
                  if (idx > 0) next[line.slice(0, idx).trim()] = line.slice(idx + 1);
                }
                updateServer(name, { env: next });
              }}
            />
          </label>
        </div>
      ))}
    </div>
  );
}

/* ── Wasm plugins editor ── */

function PluginsEditor() {
  const [plugins, setPlugins] = useState<string[]>([]);
  const [status, setStatus] = useState<string>('');
  const [loading, setLoading] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const list = await invoke<string[]>('plugin_list');
      setPlugins(list);
    } catch (e) {
      setStatus(`Error: ${e}`);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { refresh(); }, [refresh]);

  const onFile = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (!file) return;
    const name = file.name.replace(/\.wasm$/i, '');
    try {
      const buf = await file.arrayBuffer();
      const bytes = Array.from(new Uint8Array(buf));
      await invoke('plugin_install', { name, wasmBytes: bytes });
      setStatus(`Installed: ${name}`);
      refresh();
    } catch (err) {
      setStatus(`Install failed: ${err}`);
    }
  };

  const uninstall = async (name: string) => {
    try {
      await invoke('plugin_uninstall', { name });
      setStatus(`Uninstalled: ${name}`);
      refresh();
    } catch (err) {
      setStatus(`Uninstall failed: ${err}`);
    }
  };

  return (
    <div className="config-section">
      <p className="config-section-desc">
        Wasm plugins extend the agent with sandboxed code. Plugins have no
        filesystem, network, or syscall access and are fuel-limited so they
        cannot hang. Files install to <code>~/.goblin/plugins/</code>.
      </p>
      <label className="config-row">
        <span>Install .wasm</span>
        <input type="file" accept=".wasm" onChange={onFile} />
      </label>
      <button className="config-btn config-btn-sm" disabled={loading} onClick={refresh}>
        {loading ? 'Refreshing...' : 'Refresh'}
      </button>
      {status && <p className="config-saved">{status}</p>}
      {plugins.length === 0 && <p className="config-section-desc">No plugins installed.</p>}
      {plugins.map((name) => (
        <div key={name} className="config-provider-card">
          <div className="config-provider-header">
            <h4 className="config-provider-name">{name}</h4>
            <button className="config-btn-sm config-btn-danger" onClick={() => uninstall(name)}>Uninstall</button>
          </div>
        </div>
      ))}
    </div>
  );
}

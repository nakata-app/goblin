import { describe, it, expect } from 'vitest';
import { highlightMarkdown } from '../components/OutputPanel';
import { groupBy } from '../components/CommandPalette';
import { STATE_EMOJI } from '../components/GoblinCharacter';
import { formatTime } from '../components/ChatPanel';

describe('highlightMarkdown', () => {
  it('escapes HTML entities', () => {
    const result = highlightMarkdown('<script>alert("xss")</script>');
    expect(result).not.toContain('<script>');
    expect(result).toContain('&lt;script&gt;');
  });

  it('renders fenced code blocks', () => {
    const result = highlightMarkdown('```ts\nconst x = 1;\n```');
    expect(result).toContain('highlight-block');
    expect(result).toContain('highlight-lang');
    expect(result).toContain('highlight-code');
    expect(result).toContain('const x = 1;');
    expect(result).toContain('ts');
  });

  it('renders inline code', () => {
    const result = highlightMarkdown('use `const` keyword');
    expect(result).toContain('<code class="inline-code">const</code>');
  });

  it('renders bold text', () => {
    const result = highlightMarkdown('hello **world** here');
    expect(result).toContain('<strong>world</strong>');
  });

  it('renders italic text', () => {
    const result = highlightMarkdown('hello *world* here');
    expect(result).toContain('<em>world</em>');
  });

  it('renders headings', () => {
    const result = highlightMarkdown('## Section Title');
    expect(result).toContain('<h2 class="md-heading h2">');
    expect(result).toContain('Section Title');
  });

  it('renders h1', () => {
    const result = highlightMarkdown('# Top');
    expect(result).toContain('<h1 class="md-heading h1">');
  });

  it('renders horizontal rule', () => {
    const result = highlightMarkdown('---');
    expect(result).toContain('<hr class="md-hr" />');
  });

  it('renders blockquote', () => {
    const result = highlightMarkdown('> quoted text');
    expect(result).toContain('<blockquote class="md-quote">');
  });

  it('renders bullet list', () => {
    const result = highlightMarkdown('- item 1');
    expect(result).toContain('<span class="md-bullet">-</span>');
  });

  it('renders numbered list', () => {
    const result = highlightMarkdown('1. first');
    expect(result).toContain('<span class="md-number">1.</span>');
  });

  it('renders links', () => {
    const result = highlightMarkdown('[click](https://example.com)');
    expect(result).toContain('<a class="md-link" href="https://example.com"');
    expect(result).toContain('>click</a>');
  });

  it('highlights stderr markers', () => {
    const result = highlightMarkdown('error: [stderr] something');
    expect(result).toContain('<span class="shell-stderr">[stderr]</span>');
  });

  it('highlights exit code markers', () => {
    const result = highlightMarkdown('[exit code: 1]');
    expect(result).toContain('<span class="shell-exit">[exit code: 1]</span>');
  });

  it('returns newline for empty input', () => {
    const result = highlightMarkdown('');
    expect(result).toBe('\n');
  });

  it('handles unclosed code block gracefully', () => {
    const result = highlightMarkdown('```js\nvar a = 1');
    expect(result).toContain('</code></pre></div>');
  });

  it('handles bullet with indentation', () => {
    const result = highlightMarkdown('  - nested item');
    expect(result).toContain('<span class="md-bullet">-</span>');
  });

  it('handles multi-character italic markup', () => {
    const result = highlightMarkdown('text with *multiple italic* words *again*');
    expect(result).toContain('<em>multiple italic</em>');
    expect(result).toContain('<em>again</em>');
  });
});

describe('groupBy', () => {
  interface TestItem {
    category: string;
    label: string;
  }

  it('groups items by category', () => {
    const items: TestItem[] = [
      { category: 'A', label: 'a1' },
      { category: 'B', label: 'b1' },
      { category: 'A', label: 'a2' },
    ];
    const result = groupBy(items, 'category');
    expect(Object.keys(result)).toHaveLength(2);
    expect(result['A']).toHaveLength(2);
    expect(result['B']).toHaveLength(1);
    expect(result['A'][1].label).toBe('a2');
  });

  it('returns empty object for empty array', () => {
    const result = groupBy([], 'category' as any);
    expect(Object.keys(result)).toHaveLength(0);
  });

  it('handles single item', () => {
    const items = [{ category: 'X', label: 'only' }];
    const result = groupBy(items, 'category');
    expect(result['X']).toHaveLength(1);
  });

  it('preserves item order within groups', () => {
    const items = [
      { category: 'A', label: 'first' },
      { category: 'A', label: 'second' },
      { category: 'A', label: 'third' },
    ];
    const result = groupBy(items, 'category');
    expect(result['A'].map(i => i.label)).toEqual(['first', 'second', 'third']);
  });
});

describe('STATE_EMOJI', () => {
  it('has all 9 states', () => {
    const states = ['idle', 'thinking', 'reading', 'writing', 'searching', 'running', 'error', 'success', 'streaming'];
    for (const s of states) {
      expect(STATE_EMOJI).toHaveProperty(s);
      expect(STATE_EMOJI[s as keyof typeof STATE_EMOJI]).toBeTruthy();
    }
  });

  it('idle is goblin face', () => {
    expect(STATE_EMOJI.idle).toBe('👺');
  });

  it('thinking is thinking face', () => {
    expect(STATE_EMOJI.thinking).toBe('🤔');
  });

  it('reading is book', () => {
    expect(STATE_EMOJI.reading).toBe('📖');
  });

  it('writing is writing hand', () => {
    expect(STATE_EMOJI.writing).toBe('✍️');
  });

  it('searching is magnifying glass', () => {
    expect(STATE_EMOJI.searching).toBe('🔍');
  });

  it('running is lightning', () => {
    expect(STATE_EMOJI.running).toBe('⚡');
  });

  it('error is scream', () => {
    expect(STATE_EMOJI.error).toBe('😱');
  });

  it('success is cool face', () => {
    expect(STATE_EMOJI.success).toBe('😎');
  });
});

describe('formatTime', () => {
  it('formats timestamp to HH:MM', () => {
    const ts = new Date(2025, 0, 1, 14, 30, 45).getTime();
    const result = formatTime(ts);
    expect(result).toMatch(/^\d{2}:\d{2}$/);
  });

  it('formats midnight as 00:00', () => {
    const ts = new Date(2025, 0, 1, 0, 0, 0).getTime();
    const result = formatTime(ts);
    expect(result).toMatch(/^\d{2}:\d{2}$/);
  });

  it('returns different values for different times', () => {
    const ts1 = new Date(2025, 0, 1, 9, 0).getTime();
    const ts2 = new Date(2025, 0, 1, 17, 30).getTime();
    expect(formatTime(ts1)).not.toBe(formatTime(ts2));
  });
});

import { Component, type ReactNode } from 'react';

interface Props {
  children: ReactNode;
}
interface State {
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: { componentStack: string }) {
    console.error('[Goblin] React error:', error, info.componentStack);
  }

  render() {
    if (this.state.error) {
      return (
        <div style={{
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
          height: '100vh',
          background: '#0d0d0d',
          color: '#ef4444',
          fontFamily: 'monospace',
          padding: 40,
          gap: 16,
        }}>
          <div style={{ fontSize: 48 }}>💥</div>
          <div style={{ fontSize: 18, fontWeight: 700 }}>Goblin crashed</div>
          <div style={{ fontSize: 13, color: '#999', maxWidth: 500, textAlign: 'center', whiteSpace: 'pre-wrap' }}>
            {this.state.error.message}
          </div>
          <button
            onClick={() => this.setState({ error: null })}
            style={{
              background: '#ef4444',
              color: '#fff',
              border: 'none',
              padding: '8px 16px',
              borderRadius: 6,
              cursor: 'pointer',
              fontFamily: 'monospace',
            }}
          >
            Retry
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}

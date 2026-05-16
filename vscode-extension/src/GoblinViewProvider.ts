import * as vscode from 'vscode';
import * as https from 'https';
import * as http from 'http';

export class GoblinViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'goblin.chatView';
  private _view?: vscode.WebviewView;

  constructor(private readonly _extensionUri: vscode.Uri) {}

  resolveWebviewView(
    webviewView: vscode.WebviewView,
    _context: vscode.WebviewViewResolveContext,
    _token: vscode.CancellationToken,
  ) {
    this._view = webviewView;

    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this._extensionUri, 'media')],
    };

    webviewView.webview.html = this._buildHtml(webviewView.webview);

    webviewView.webview.onDidReceiveMessage(async (msg) => {
      if (msg.type === 'send') {
        await this._sendToGoblin(msg.text);
      } else if (msg.type === 'ready') {
        await this._checkConnection();
      }
    });
  }

  /** Called by the clearChat command */
  clear() {
    this._view?.webview.postMessage({ type: 'clear' });
  }

  /** Check if Goblin HTTP API is reachable */
  async reconnect() {
    await this._checkConnection();
  }

  /** Inject text from editor selection into the input box */
  inject(text: string) {
    this._view?.webview.postMessage({ type: 'inject', text });
    this._view?.show?.(true);
  }

  private cfg() {
    const c = vscode.workspace.getConfiguration('goblin');
    return {
      host: c.get<string>('host', '127.0.0.1'),
      port: c.get<number>('port', 1789),
      token: c.get<string>('token', ''),
    };
  }

  private async _checkConnection() {
    const { host, port, token } = this.cfg();
    if (!token) {
      this._postStatus(false, 'Token ayarlanmamış — Goblin > Settings');
      return;
    }
    try {
      await this._request('GET', host, port, token, '/health', null);
      this._postStatus(true, `${host}:${port}`);
    } catch {
      this._postStatus(false, `Goblin kapalı — ${host}:${port}`);
    }
  }

  private _postStatus(connected: boolean, label: string) {
    this._view?.webview.postMessage({ type: 'status', connected, label });
  }

  private async _sendToGoblin(text: string) {
    const { host, port, token } = this.cfg();

    if (!token) {
      this._view?.webview.postMessage({
        type: 'error',
        text: 'Token ayarlı değil. VS Code Ayarlar > Goblin > Token.',
      });
      return;
    }

    try {
      const body = JSON.stringify({ text });
      const data = await this._request('POST', host, port, token, '/message', body);
      this._view?.webview.postMessage({
        type: 'response',
        content: data.content ?? '(boş yanıt)',
        model: data.model ?? '',
        tokens_in: data.tokens_in ?? 0,
        tokens_out: data.tokens_out ?? 0,
      });
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      this._view?.webview.postMessage({ type: 'error', text: msg });
    }
  }

  /** Minimal Node.js http/https request (no external deps) */
  private _request(
    method: string,
    host: string,
    port: number,
    token: string,
    path: string,
    body: string | null,
  ): Promise<Record<string, unknown>> {
    return new Promise((resolve, reject) => {
      const isHttps = port === 443;
      const options: http.RequestOptions = {
        hostname: host,
        port,
        path,
        method,
        headers: {
          Authorization: `Bearer ${token}`,
          'Content-Type': 'application/json',
          ...(body ? { 'Content-Length': Buffer.byteLength(body) } : {}),
        },
        timeout: 120_000,
      };

      const transport = isHttps ? https : http;
      const req = transport.request(options, (res) => {
        let raw = '';
        res.on('data', (chunk) => { raw += chunk; });
        res.on('end', () => {
          if (!res.statusCode || res.statusCode < 200 || res.statusCode >= 300) {
            reject(new Error(`HTTP ${res.statusCode}: ${raw}`));
            return;
          }
          try {
            resolve(JSON.parse(raw));
          } catch {
            reject(new Error(`JSON parse hatası: ${raw.slice(0, 200)}`));
          }
        });
      });

      req.on('error', (e) => reject(new Error(`Bağlantı hatası: ${e.message}`)));
      req.on('timeout', () => {
        req.destroy();
        reject(new Error('Goblin yanıt vermedi (120s timeout)'));
      });

      if (body) req.write(body);
      req.end();
    });
  }

  private _buildHtml(webview: vscode.Webview): string {
    const styleUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this._extensionUri, 'media', 'style.css'),
    );
    const scriptUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this._extensionUri, 'media', 'main.js'),
    );
    const nonce = getNonce();

    return /* html */ `<!DOCTYPE html>
<html lang="tr">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy"
    content="default-src 'none';
             style-src ${webview.cspSource};
             script-src 'nonce-${nonce}';">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <link rel="stylesheet" href="${styleUri}">
  <title>Goblin</title>
</head>
<body>
  <div id="status-bar">
    <span id="status-dot"></span>
    <span id="status-text">Bağlanıyor...</span>
  </div>

  <div id="messages"></div>

  <div id="input-area">
    <div id="input-wrap">
      <textarea
        id="input"
        rows="1"
        placeholder="Goblin'e yaz… (Enter gönder, Shift+Enter satır)"
        autocomplete="off"
        spellcheck="false"
      ></textarea>
      <button id="send-btn" title="Gönder">↑</button>
    </div>
    <div id="input-hint">Cmd+Shift+G ile seçili kodu buraya gönder</div>
  </div>

  <script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`;
  }
}

function getNonce() {
  let text = '';
  const possible = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  for (let i = 0; i < 32; i++) {
    text += possible.charAt(Math.floor(Math.random() * possible.length));
  }
  return text;
}

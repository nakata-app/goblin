import * as vscode from 'vscode';
import { GoblinViewProvider } from './GoblinViewProvider';

export function activate(context: vscode.ExtensionContext) {
  const provider = new GoblinViewProvider(context.extensionUri);

  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(GoblinViewProvider.viewType, provider, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('goblin.clearChat', () => {
      provider.clear();
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('goblin.reconnect', () => {
      provider.reconnect();
    }),
  );

  // Cmd+Shift+G: seçili kodu veya aktif dosyayı Goblin input'una enjekte et
  context.subscriptions.push(
    vscode.commands.registerCommand('goblin.sendSelection', () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) return;

      const selection = editor.selection;
      const text = editor.document.getText(selection.isEmpty ? undefined : selection);
      const lang = editor.document.languageId;
      const filename = editor.document.fileName.split('/').pop() ?? '';

      const prefix = selection.isEmpty
        ? `# ${filename}\n\`\`\`${lang}\n`
        : `# ${filename} (seçim)\n\`\`\`${lang}\n`;

      provider.inject(`${prefix}${text}\n\`\`\`\n\n`);
    }),
  );
}

export function deactivate() {}

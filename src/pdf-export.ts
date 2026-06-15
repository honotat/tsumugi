import { save } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { renderMarkdown } from './renderer';
import { collectStyles, buildDocument } from './html-export';

export async function exportAsPdf(title: string, markdown: string): Promise<void> {
  const filePath = await save({
    defaultPath: `${title}.pdf`,
    filters: [{ name: 'PDF', extensions: ['pdf'] }],
  });
  if (!filePath) return;

  // 編集モード中は表示DOMがtextareaのためMarkdownから再レンダリング
  const container = document.createElement('div');
  container.style.position = 'fixed';
  container.style.left = '-9999px';
  document.body.appendChild(container);

  try {
    await renderMarkdown(markdown, container);
    container.querySelectorAll('.code-copy-btn').forEach(btn => btn.remove());

    const styles = collectStyles() + getPdfExtraCss();
    const html = buildDocument(title, 'light', styles, container.innerHTML);

    await invoke('export_pdf', { htmlContent: html, outputPath: filePath });
  } finally {
    document.body.removeChild(container);
  }
}

function getPdfExtraCss(): string {
  return `
@page { margin: 15mm; }
.code-copy-btn { display: none !important; }
h1, h2, h3 { break-after: avoid; }
pre, table, .mermaid-container { break-inside: avoid; }
img { max-width: 100% !important; }
body { -webkit-print-color-adjust: exact; print-color-adjust: exact; }
`;
}

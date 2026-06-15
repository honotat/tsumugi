import { renderMarkdown } from '../renderer';
import { openUrl } from '@tauri-apps/plugin-opener';

/**
 * EditorController はビューモード、エディットモード、スプリットモードを管理する。
 * ビューモード: renderMarkdown() でレンダリングし、フォーム要素は操作可能。
 * エディットモード: 1つの textarea に raw Markdown 全体を表示して編集。
 * スプリットモード: 左にエディタ、右にプレビューをリアルタイム表示。
 */
export class EditorController {
  private container: HTMLElement;
  private mode: 'view' | 'edit' | 'split' = 'view';
  private currentContent: string = '';
  private onContentChange: ((markdown: string) => void) | null = null;
  private onModeChange: ((mode: 'view' | 'edit' | 'split') => void) | null = null;

  /** Undo/Redo スタック（markdown スナップショット）*/
  private undoStack: string[] = [];
  private redoStack: string[] = [];

  /** スプリットモードのプレビューコンテナ */
  private splitPreviewContainer: HTMLElement | null = null;
  /** スプリットプレビューのデバウンスタイマー */
  private splitPreviewTimer: ReturnType<typeof setTimeout> | null = null;
  /** textarea 監視用 ResizeObserver */
  private resizeObserver: ResizeObserver | null = null;

  constructor(container: HTMLElement) {
    this.container = container;
    this.container.addEventListener('click', (e) => this.handleLinkClick(e));
  }

  /** レンダリング結果内のリンククリックを横取りし外部URLは既定アプリで開く */
  private handleLinkClick(e: MouseEvent): void {
    const anchor = (e.target as HTMLElement)?.closest('a');
    if (!anchor) return;
    const href = anchor.getAttribute('href');
    if (!href) return;
    // ページ内アンカーは従来どおりアプリ内スクロールに任せる
    if (href.startsWith('#')) return;
    // file: やOS独自スキームを既定ハンドラに渡さないようスキームを限定する
    let url: URL;
    try {
      url = new URL(href, window.location.href);
    } catch {
      return;
    }
    const allowed = ['http:', 'https:', 'mailto:', 'tel:'];
    if (!allowed.includes(url.protocol)) return;
    e.preventDefault();
    openUrl(url.toString()).catch(() => {});
  }

  /** コンテンツ変更時のコールバックを設定 */
  setOnContentChange(cb: (markdown: string) => void): void {
    this.onContentChange = cb;
  }

  /** モード変更時のコールバックを設定 */
  setOnModeChange(cb: (mode: 'view' | 'edit' | 'split') => void): void {
    this.onModeChange = cb;
  }

  /** エディタ開始（ビューモードで表示） */
  enterEditMode(content: string): void {
    this.currentContent = content;
    this.mode = 'view';
    document.body.classList.add('edit-mode');
    this.undoStack = [content];
    this.redoStack = [];
    this.renderView();
  }

  /** エディタ終了 */
  exitEditMode(): string {
    this.syncFormInputs();
    this.syncFromEdit();
    this.disconnectResizeObserver();
    document.body.classList.remove('edit-mode');
    document.body.classList.remove('split-mode');
    return this.currentContent;
  }

  /** 現在の Markdown を返す */
  getCurrentContent(): string {
    this.syncFormInputs();
    this.syncFromEdit();
    return this.currentContent;
  }

  /** 外部からの更新（ファイル監視等） */
  updateContent(content: string): void {
    this.currentContent = content;
    this.undoStack = [content];
    this.redoStack = [];
    if (this.mode === 'view') {
      this.renderView();
    } else if (this.mode === 'split') {
      this.updateTextarea(content);
      this.renderSplitPreview();
    } else {
      this.updateTextarea(content);
    }
  }

  /** ビューモードに切替 */
  switchToView(): void {
    if (this.mode === 'view') return;
    this.syncFromEdit();
    this.splitPreviewContainer = null;
    document.body.classList.remove('split-mode');
    this.mode = 'view';
    // undoスタックにエディット結果を追加（変更がある場合）
    if (this.undoStack.length === 0 || this.undoStack[this.undoStack.length - 1] !== this.currentContent) {
      this.undoStack.push(this.currentContent);
      this.redoStack = [];
      if (this.undoStack.length > 100) {
        this.undoStack.shift();
      }
    }
    this.renderView();
    if (this.onModeChange) this.onModeChange('view');
  }

  /** エディットモードに切替 */
  switchToEdit(): void {
    if (this.mode === 'edit') return;
    this.syncFromEdit();
    this.syncFormInputs();
    this.splitPreviewContainer = null;
    document.body.classList.remove('split-mode');
    this.mode = 'edit';
    this.renderEdit();
    if (this.onModeChange) this.onModeChange('edit');
  }

  /** スプリットモードに切替 */
  switchToSplit(): void {
    if (this.mode === 'split') return;
    this.syncFromEdit();
    this.syncFormInputs();
    document.body.classList.add('split-mode');
    this.mode = 'split';
    this.renderSplit();
    if (this.onModeChange) this.onModeChange('split');
  }

  /** 最後の変更を元に戻す */
  undo(): void {
    if (this.undoStack.length <= 1) return;
    const current = this.undoStack.pop()!;
    this.redoStack.push(current);
    const prev = this.undoStack[this.undoStack.length - 1];
    this.currentContent = prev;
    if (this.mode === 'view') {
      this.renderView();
    } else {
      this.updateTextarea(prev);
      if (this.mode === 'split') this.debouncedRenderSplitPreview();
    }
    if (this.onContentChange) this.onContentChange(prev);
  }

  /** 最後に戻した変更をやり直す */
  redo(): void {
    if (this.redoStack.length === 0) return;
    const next = this.redoStack.pop()!;
    this.undoStack.push(next);
    this.currentContent = next;
    if (this.mode === 'view') {
      this.renderView();
    } else {
      this.updateTextarea(next);
      if (this.mode === 'split') this.debouncedRenderSplitPreview();
    }
    if (this.onContentChange) this.onContentChange(next);
  }

  // --- ビューモード ---

  private renderView(): void {
    this.disconnectResizeObserver();
    renderMarkdown(this.currentContent, this.container).then(() => {
      this.attachFormEvents();
    });
  }

  private attachFormEvents(target?: HTMLElement): void {
    const container = target || this.container;

    // チェックボックス
    const checkboxes = container.querySelectorAll('input[type="checkbox"]');
    checkboxes.forEach((cb, index) => {
      (cb as HTMLInputElement).removeAttribute('disabled');
      cb.addEventListener('click', (e) => {
        e.preventDefault();
        e.stopPropagation();
        this.syncFormInputs();
        this.toggleCheckbox(index);
      });
    });

    // ラジオボタン
    const radios = container.querySelectorAll('input[type="radio"]');
    radios.forEach((radio, index) => {
      const groupName = (radio as HTMLInputElement).name;
      const groupIndices: number[] = [];
      radios.forEach((r, idx) => {
        if ((r as HTMLInputElement).name === groupName) {
          groupIndices.push(idx);
        }
      });
      (radio as HTMLInputElement).removeAttribute('disabled');
      radio.addEventListener('click', (e) => {
        e.preventDefault();
        e.stopPropagation();
        this.syncFormInputs();
        this.toggleRadio(index, groupIndices);
      });
    });

    // テキスト入力
    const textInputs = container.querySelectorAll('input[type="text"].deflist-text-input');
    textInputs.forEach((input) => {
      input.addEventListener('keydown', (e) => e.stopPropagation());
      input.addEventListener('change', () => {
        this.syncFormInputs();
        if (this.mode === 'split') {
          this.updateTextarea(this.currentContent);
        }
        this.notifyChange();
      });
    });
  }

  // --- エディットモード ---

  private renderEdit(): void {
    this.container.textContent = '';
    this.buildEditUI(this.container, false);
  }

  /** スプリットモードのレンダリング */
  private renderSplit(): void {
    this.container.textContent = '';

    const wrapper = document.createElement('div');
    wrapper.className = 'split-wrapper';

    // 左ペイン: エディタ
    const editPane = document.createElement('div');
    editPane.className = 'split-pane split-edit-pane';
    this.buildEditUI(editPane, true);

    // 右ペイン: プレビュー
    const previewPane = document.createElement('div');
    previewPane.className = 'split-pane split-preview-pane';
    const previewContent = document.createElement('div');
    previewContent.className = 'split-preview-content';
    previewPane.appendChild(previewContent);
    this.splitPreviewContainer = previewContent;

    wrapper.appendChild(editPane);
    wrapper.appendChild(previewPane);
    this.container.appendChild(wrapper);

    this.renderSplitPreview();
  }

  /** スプリットプレビューパネルのみ再レンダリング */
  private renderSplitPreview(): void {
    if (!this.splitPreviewContainer) return;
    renderMarkdown(this.currentContent, this.splitPreviewContainer).then(() => {
      if (this.splitPreviewContainer) {
        this.attachFormEvents(this.splitPreviewContainer);
      }
    });
  }

  /** デバウンス付きスプリットプレビュー更新 */
  private debouncedRenderSplitPreview(): void {
    if (this.splitPreviewTimer) clearTimeout(this.splitPreviewTimer);
    this.splitPreviewTimer = setTimeout(() => {
      this.splitPreviewTimer = null;
      this.renderSplitPreview();
    }, 300);
  }

  /**
   * エディタUI（行番号 + textarea）を指定コンテナに構築する。
   * isSplit=true の場合、input イベントでスプリットプレビューも更新する。
   */
  private buildEditUI(target: HTMLElement, isSplit: boolean): void {
    const wrapper = document.createElement('div');
    wrapper.className = 'editor-wrapper';

    const lineNumbers = document.createElement('div');
    lineNumbers.className = 'line-numbers';

    const textarea = document.createElement('textarea');
    textarea.className = 'editor-textarea';
    textarea.value = this.currentContent;
    textarea.spellcheck = false;

    const applyInput = () => {
      this.currentContent = textarea.value;
      this.updateLineNumbers();
      this.autoResizeTextarea();
      if (isSplit) this.debouncedRenderSplitPreview();
      if (this.onContentChange) {
        this.onContentChange(this.currentContent);
      }
    };
    textarea.addEventListener('input', (e) => {
      // IME変換中は表示更新のみ行う
      if ((e as InputEvent).isComposing) {
        this.currentContent = textarea.value;
        this.updateLineNumbers();
        this.autoResizeTextarea();
        return;
      }
      applyInput();
    });
    textarea.addEventListener('compositionend', applyInput);

    // スクロール同期（行番号とtextarea間、フォールバック用）
    textarea.addEventListener('scroll', () => {
      lineNumbers.scrollTop = textarea.scrollTop;
    });

    wrapper.appendChild(lineNumbers);
    wrapper.appendChild(textarea);
    target.appendChild(wrapper);

    this.updateLineNumbers();
    this.autoResizeTextarea();

    // リサイズで折り返しが変わるため行番号を再計算する
    this.disconnectResizeObserver();
    this.resizeObserver = new ResizeObserver(() => {
      this.updateLineNumbers();
      this.autoResizeTextarea();
    });
    this.resizeObserver.observe(textarea);

    textarea.focus();
  }

  /** textarea 監視用 ResizeObserver を解放する */
  private disconnectResizeObserver(): void {
    if (this.resizeObserver) {
      this.resizeObserver.disconnect();
      this.resizeObserver = null;
    }
  }

  /** textarea の値を更新し行番号も再描画する共通ヘルパー */
  private updateTextarea(content: string): void {
    const textarea = this.container.querySelector('.editor-textarea') as HTMLTextAreaElement;
    if (textarea) {
      textarea.value = content;
      this.updateLineNumbers();
      this.autoResizeTextarea();
    }
  }

  /** textarea をコンテンツに合わせて自動リサイズする */
  private autoResizeTextarea(): void {
    const textarea = this.container.querySelector('.editor-textarea') as HTMLTextAreaElement;
    if (!textarea) return;
    textarea.style.height = 'auto';
    textarea.style.height = textarea.scrollHeight + 'px';
  }

  /** 行番号を textarea の内容に合わせて更新（折り返し対応） */
  private updateLineNumbers(): void {
    const lineNumbers = this.container.querySelector('.line-numbers');
    const textarea = this.container.querySelector('.editor-textarea') as HTMLTextAreaElement;
    if (!lineNumbers || !textarea) return;

    const lines = textarea.value.split('\n');
    const styles = window.getComputedStyle(textarea);
    const lineHeight = parseFloat(styles.lineHeight);

    // ミラー要素でtextareaと同じ折り返しを再現し、各行の表示行数を測定する
    const mirror = document.createElement('div');
    mirror.style.position = 'absolute';
    mirror.style.visibility = 'hidden';
    mirror.style.whiteSpace = 'pre-wrap';
    mirror.style.wordWrap = 'break-word';
    mirror.style.overflow = 'hidden';
    for (const prop of ['fontFamily', 'fontSize', 'fontWeight', 'letterSpacing', 'lineHeight', 'padding', 'width', 'borderWidth', 'boxSizing'] as const) {
      (mirror.style as any)[prop] = (styles as any)[prop];
    }
    document.body.appendChild(mirror);

    const fragment = document.createDocumentFragment();
    for (let i = 0; i < lines.length; i++) {
      mirror.textContent = lines[i] || '\u00a0';
      const visualLines = Math.max(1, Math.round(mirror.offsetHeight / lineHeight));

      const span = document.createElement('span');
      span.textContent = String(i + 1);
      if (visualLines > 1) {
        span.style.height = (lineHeight * visualLines) + 'px';
      }
      fragment.appendChild(span);
    }

    document.body.removeChild(mirror);
    lineNumbers.textContent = '';
    lineNumbers.appendChild(fragment);
  }

  // --- フォーム操作 ---

  /** テキスト入力の値を currentContent に同期 */
  private syncFormInputs(): void {
    if (this.mode === 'edit') return;
    const target = this.mode === 'split' ? this.splitPreviewContainer : this.container;
    if (!target) return;
    const textInputs = target.querySelectorAll('input[type="text"].deflist-text-input');
    if (textInputs.length === 0) return;

    let count = 0;
    this.currentContent = this.currentContent.replace(/\[T:"([^"]*)"\]/g, (match) => {
      const input = textInputs[count] as HTMLInputElement | undefined;
      count++;
      if (input) {
        const safeValue = input.value.replace(/"/g, '');
        return `[T:"${safeValue}"]`;
      }
      return match;
    });
  }

  /** エディット/スプリットモードの textarea の値を currentContent に同期 */
  private syncFromEdit(): void {
    if (this.mode !== 'edit' && this.mode !== 'split') return;
    const textarea = this.container.querySelector('.editor-textarea') as HTMLTextAreaElement;
    if (textarea) this.currentContent = textarea.value;
  }

  private toggleCheckbox(index: number): void {
    let count = 0;
    this.currentContent = this.currentContent.replace(/\[([ xX])\]/g, (match, state) => {
      if (count === index) {
        count++;
        return state === ' ' ? '[x]' : '[ ]';
      }
      count++;
      return match;
    });
    if (this.mode === 'split') {
      this.updateTextarea(this.currentContent);
      this.renderSplitPreview();
    } else {
      this.renderView();
    }
    this.notifyChange();
  }

  private toggleRadio(clickedIndex: number, groupIndices: number[]): void {
    let count = 0;
    this.currentContent = this.currentContent.replace(/\[R:"(x?)"\]/g, (match) => {
      const idx = count++;
      if (groupIndices.includes(idx)) {
        return idx === clickedIndex ? '[R:"x"]' : '[R:""]';
      }
      return match;
    });
    if (this.mode === 'split') {
      this.updateTextarea(this.currentContent);
      this.renderSplitPreview();
    } else {
      this.renderView();
    }
    this.notifyChange();
  }

  private notifyChange(): void {
    if (this.undoStack.length === 0 || this.undoStack[this.undoStack.length - 1] !== this.currentContent) {
      this.undoStack.push(this.currentContent);
      this.redoStack = [];
      if (this.undoStack.length > 100) {
        this.undoStack.shift();
      }
    }
    if (this.onContentChange) {
      this.onContentChange(this.currentContent);
    }
  }
}

#!/usr/bin/env python3
"""tsumugi MCP Server — Claude Code / Claude Desktop から tsumugi を操作する."""

import logging
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import httpx
from mcp.server.fastmcp import FastMCP

logging.basicConfig(
    level=logging.INFO,
    format="%(name)s - %(levelname)s - %(message)s",
    handlers=[logging.StreamHandler(sys.stderr)],
)
logger = logging.getLogger("tsumugi-mcp")

mcp = FastMCP(name="tsumugi")


def _instance_dir() -> Path:
    """プラットフォームに応じたインスタンスディレクトリを返す."""
    # Rust側の temp_dir() と基準を揃える
    base = Path(tempfile.gettempdir())
    if sys.platform == "win32":
        username = os.environ.get("USERNAME", "default")
        return base / f"tsumugi-{username}"
    else:
        uid = os.getuid()
        return base / f"tsumugi-{uid}"


def _read_connection(instance_id: str) -> tuple[str, str]:
    """ポートファイルからURL・トークンを読み取る."""
    if not re.match(r'^[a-zA-Z0-9_-]+$', instance_id):
        raise ValueError(f"Invalid instance_id: {instance_id!r}")
    port_file = _instance_dir() / f"{instance_id}.http"
    if not port_file.exists():
        raise FileNotFoundError(f"tsumugi instance '{instance_id}' not found: {port_file}")
    info = port_file.read_text().strip()
    port_str, token = info.split(":", 1)
    port = int(port_str)
    if not (1 <= port <= 65535):
        raise ValueError(f"Invalid port number: {port_str}")
    return f"http://127.0.0.1:{port}", token


def _client(instance_id: str) -> httpx.Client:
    """認証付きHTTPクライアントを生成する."""
    base_url, token = _read_connection(instance_id)
    return httpx.Client(
        base_url=base_url,
        headers={"Authorization": f"Bearer {token}"},
        timeout=30.0,
    )


def _list_instances() -> list[str]:
    """利用可能なインスタンスIDの一覧を返す."""
    d = _instance_dir()
    if not d.exists():
        return []
    return [f.stem for f in d.glob("*.http")]


def _resolve_instance(instance_id: str | None) -> str:
    """インスタンスIDを解決する. 省略時は唯一のインスタンスを自動選択."""
    if instance_id:
        return instance_id
    instances = _list_instances()
    if len(instances) == 1:
        return instances[0]
    if len(instances) == 0:
        raise RuntimeError("tsumugi instance not found. Launch tsumugi first.")
    raise RuntimeError(
        f"Multiple tsumugi instances running: {instances}. Specify instance_id."
    )


def _find_tsumugi_bin() -> str:
    """tsumugi バイナリのパスを探す."""
    # PATH から探す
    found = shutil.which("tsumugi")
    if found:
        return found
    # PATH に無い場合のインストール先をプラットフォーム別に探す
    if sys.platform == "win32":
        candidates = [
            Path.home() / ".cargo" / "bin" / "tsumugi.exe",
            Path(os.environ.get("LOCALAPPDATA", str(Path.home() / "AppData" / "Local")))
            / "Microsoft" / "WinGet" / "Links" / "tsumugi.exe",
            Path.home() / "scoop" / "shims" / "tsumugi.exe",
        ]
    else:
        candidates = [
            Path.home() / ".cargo" / "bin" / "tsumugi",
            Path("/usr/local/bin/tsumugi"),
            Path("/opt/homebrew/bin/tsumugi"),
        ]
    for p in candidates:
        if p.exists():
            return str(p)
    raise FileNotFoundError(
        "tsumugi binary not found. Install tsumugi or add it to PATH."
    )


# --- Tools ---


def _launch_process(title: str | None = None) -> str:
    """tsumugiプロセスを起動してインスタンスIDを返す（ポートファイル待機含む）."""
    bin_path = _find_tsumugi_bin()
    cmd: list[str] = [bin_path]
    if title:
        cmd += ["--title", title]

    proc = subprocess.Popen(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True
    )
    auto_id = proc.stdout.readline().strip()
    proc.stdout.close()
    if not auto_id:
        proc.wait(timeout=10)
        raise RuntimeError(
            f"Failed to launch tsumugi (exit code {proc.returncode})"
        )

    # ポートファイルが生成されるまで待機
    port_file = _instance_dir() / f"{auto_id}.http"
    for _ in range(20):
        if port_file.exists():
            break
        time.sleep(0.25)
    else:
        # 待機しても接続情報が書き出されなかった場合は失敗とみなす
        raise RuntimeError(
            f"tsumugi launched (id={auto_id}) but its connection file did not "
            f"appear within 5s: {port_file}"
        )

    return auto_id


@mcp.tool()
def launch(body: str = "", title: str | None = None) -> str:
    """新しい tsumugi ウィンドウを開いてコンテンツを表示する.

    新規ウィンドウを開き、自動生成されたインスタンスIDを返す。
    返されたIDを使って update / query 等で操作できる。

    Args:
        body: 表示する Markdown 本文（省略時は空のウィンドウ）
        title: ドキュメントタイトル（省略時は "Untitled"）
    """
    # bodyが空の場合はコマンドライン漏洩リスクなし — 従来通り起動
    if not body:
        return _launch_process(title=title)

    # 既存インスタンスがあれば CreateWindow API で新ウィンドウを開く
    instances = _list_instances()
    if instances:
        for iid in instances:
            try:
                with _client(iid) as c:
                    r = c.post("/", json={
                        "type": "CreateWindow",
                        "body": body,
                        "title": title,
                    })
                    data = r.json()
                    if data.get("ok"):
                        return data.get("value", "")
            except Exception:
                continue

    # 既存インスタンスなし — bodyなしで起動し、API経由でbodyを送信
    auto_id = _launch_process(title=title)
    try:
        with _client(auto_id) as c:
            c.post(
                "/update",
                content=body.encode("utf-8"),
                headers={"Content-Type": "text/markdown"},
                params={"title": title} if title else None,
            )
    except Exception:
        logger.warning("Failed to send body via API, window may be empty")

    return auto_id


@mcp.tool()
def update(body: str, title: str | None = None, instance_id: str | None = None) -> str:
    """tsumugi ウィンドウの表示内容を更新する.

    Args:
        body: 表示する Markdown 本文
        title: ドキュメントタイトル（省略時は変更しない）
        instance_id: 対象インスタンスID（省略時は自動選択）
    """
    iid = _resolve_instance(instance_id)
    with _client(iid) as c:
        r = c.post(
            "/update",
            content=body.encode("utf-8"),
            headers={"Content-Type": "text/markdown"},
            params={"title": title} if title else None,
        )
        return r.text


@mcp.tool()
def query(
    properties: list[str] | None = None, instance_id: str | None = None
) -> str:
    """tsumugi ウィンドウのプロパティを取得する.

    Args:
        properties: 取得するプロパティ (body, title, path, status, linecount, all)
        instance_id: 対象インスタンスID（省略時は自動選択）
    """
    iid = _resolve_instance(instance_id)
    props = properties or ["all"]
    with _client(iid) as c:
        r = c.post("/query", json={"properties": props})
        return r.text


@mcp.tool()
def grep(pattern: str, instance_id: str | None = None) -> str:
    """tsumugi ウィンドウの本文を正規表現で検索する.

    Args:
        pattern: 検索する正規表現パターン
        instance_id: 対象インスタンスID（省略時は自動選択）
    """
    iid = _resolve_instance(instance_id)
    with _client(iid) as c:
        r = c.post(
            "/grep",
            content=pattern.encode("utf-8"),
            headers={"Content-Type": "text/plain"},
        )
        return r.text


@mcp.tool()
def get_lines(start: int, end: int, instance_id: str | None = None) -> str:
    """tsumugi ウィンドウの指定行範囲を取得する.

    Args:
        start: 開始行番号（1始まり）
        end: 終了行番号（1始まり、両端含む）
        instance_id: 対象インスタンスID（省略時は自動選択）
    """
    iid = _resolve_instance(instance_id)
    with _client(iid) as c:
        r = c.post("/lines", json={"start": start, "end": end})
        return r.text


@mcp.tool()
def edit_insert(line: int, content: str, instance_id: str | None = None) -> str:
    """tsumugi ウィンドウの指定行の前にコンテンツを挿入する.

    Args:
        line: 挿入位置の行番号（1始まり、この行の前に挿入）
        content: 挿入する Markdown コンテンツ
        instance_id: 対象インスタンスID（省略時は自動選択）
    """
    iid = _resolve_instance(instance_id)
    with _client(iid) as c:
        r = c.post(
            "/edit/insert",
            content=content.encode("utf-8"),
            headers={"Content-Type": "text/markdown"},
            params={"line": str(line)},
        )
        return r.text


@mcp.tool()
def edit_replace(
    start: int, end: int, content: str, instance_id: str | None = None
) -> str:
    """tsumugi ウィンドウの指定行範囲を新しいコンテンツで置換する.

    Args:
        start: 開始行番号（1始まり）
        end: 終了行番号（1始まり、両端含む）
        content: 置換後の Markdown コンテンツ
        instance_id: 対象インスタンスID（省略時は自動選択）
    """
    iid = _resolve_instance(instance_id)
    with _client(iid) as c:
        r = c.post(
            "/edit/replace",
            content=content.encode("utf-8"),
            headers={"Content-Type": "text/markdown"},
            params={"start": str(start), "end": str(end)},
        )
        return r.text


@mcp.tool()
def edit_delete(ranges: list[list[int]], instance_id: str | None = None) -> str:
    """tsumugi ウィンドウの指定行を削除する.

    Args:
        ranges: 削除する行範囲のリスト（例: [[5, 8], [12, 12]]）
        instance_id: 対象インスタンスID（省略時は自動選択）
    """
    iid = _resolve_instance(instance_id)
    with _client(iid) as c:
        r = c.post("/edit/delete", json={"ranges": ranges})
        return r.text


@mcp.tool()
def list_instances() -> str:
    """起動中の tsumugi インスタンスの一覧を取得する."""
    instances = _list_instances()
    if not instances:
        return "No tsumugi instances running."
    results = []
    for iid in instances:
        try:
            with _client(iid) as c:
                r = c.get("/health")
                results.append(f"{iid}: running (v{r.json().get('value', '?')})")
        except Exception:
            results.append(f"{iid}: not responding")
    return "\n".join(results)


@mcp.tool()
def health(instance_id: str | None = None) -> str:
    """tsumugi インスタンスのヘルスチェック.

    Args:
        instance_id: 対象インスタンスID（省略時は自動選択）
    """
    iid = _resolve_instance(instance_id)
    with _client(iid) as c:
        r = c.get("/health")
        return r.text


if __name__ == "__main__":
    mcp.run(transport="stdio")

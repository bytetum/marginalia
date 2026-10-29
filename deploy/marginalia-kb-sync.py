#!/usr/bin/env python3
"""Copy Marginalia highlights into a kb store, as an outline:

    Marginalia highlights            (mg-root)
      <article title>  #article      (mg-a-<article id>)  url, reader
        <highlight text>  #highlight (mg-h-<highlight id>) url, reader, note, highlighted

Only articles with at least one highlight are copied. Node ids are derived from Marginalia ids, so a run only adds what
is missing: it never duplicates, edits or deletes, and nodes moved, edited or deleted in kb stay as you left them.
Ids already copied are remembered in STATE_FILE (one line per kb node id), so a run with nothing new makes no kb calls;
delete that file to re-check every node against kb.

Reads MARGINALIA_BIND / MARGINALIA_TOKEN from ~/.config/marginalia/env (or the environment); writes with the kb CLI to
KB_ROOT (default: the kb home store, ~/.local/share/kb/home).
"""

import json
import os
import subprocess
import sys
import urllib.request
from pathlib import Path

HOME = Path.home()
ENV_FILE = Path(os.environ.get("MARGINALIA_ENV", HOME / ".config/marginalia/env"))
KB_ROOT = os.environ.get("KB_ROOT", str(HOME / ".local/share/kb/home"))

TAGS = {"tag.article": "article", "tag.highlight": "highlight"}
FIELDS = {"f.url": ("url", "url"), "f.reader": ("reader", "url"), "f.note": ("note", "text"),
          "f.highlighted": ("highlighted", "date")}
ROOT_ID = "mg-root"
STATE_FILE = Path(os.environ.get("MARGINALIA_KB_STATE", HOME / ".local/state/marginalia/kb-synced.txt"))


def env(name: str, default: str = "") -> str:
    if name in os.environ:
        return os.environ[name]
    if ENV_FILE.exists():
        for line in ENV_FILE.read_text().splitlines():
            key, sep, value = line.partition("=")
            if sep and key.strip() == name:
                return value.strip()
    return default


def kb(*args: str, check: bool = True) -> dict:
    out = subprocess.run(["kb", "--root", KB_ROOT, "--json", *args], capture_output=True, text=True)
    try:
        result = json.loads(out.stdout)
    except json.JSONDecodeError:
        result = {"status": "failed", "message": (out.stderr or out.stdout).strip()}
    if check and result.get("status") != "succeeded":
        raise RuntimeError(f"kb {' '.join(args[:2])}: {result.get('message', result)}")
    return result


def exists(node_id: str) -> bool:
    return kb("get", node_id, check=False).get("status") == "succeeded"


def ensure_schema() -> None:
    for tag_id, name in TAGS.items():
        if not exists(tag_id):
            kb("tag", "define", name, "--id", tag_id)
    for field_id, (name, kind) in FIELDS.items():
        if not exists(field_id):
            kb("field", "define", name, "--id", field_id)
            kb("field", "type", field_id, kind)
    if not exists(ROOT_ID):
        kb("add", "Marginalia highlights", "--id", ROOT_ID)


def load_state() -> set:
    return set(STATE_FILE.read_text().split()) if STATE_FILE.exists() else set()


def remember(done: set, node_id: str) -> None:
    done.add(node_id)
    STATE_FILE.parent.mkdir(parents=True, exist_ok=True)
    with STATE_FILE.open("a") as f:
        f.write(node_id + "\n")


def main() -> int:
    bind = env("MARGINALIA_BIND", "127.0.0.1:7878").replace("0.0.0.0", "127.0.0.1")
    base = f"http://{bind}"
    req = urllib.request.Request(f"{base}/export", headers={"Authorization": f"Bearer {env('MARGINALIA_TOKEN')}"})
    with urllib.request.urlopen(req, timeout=60) as resp:
        library = json.load(resp)

    articles = {a["id"]: a for a in library["articles"]}
    highlights = sorted(library["highlights"], key=lambda h: h["created_at"])
    done = load_state()
    todo = [h for h in highlights if f"mg-h-{h['id']}" not in done and h["article_id"] in articles]
    if todo and ROOT_ID not in done:
        ensure_schema()
        remember(done, ROOT_ID)

    added_articles = added_highlights = 0
    for h in todo:
        article = articles[h["article_id"]]
        article_node = f"mg-a-{article['id']}"
        if article_node in done:
            pass
        elif exists(article_node):
            remember(done, article_node)
        else:
            kb("add", article["title"] or article["url"], "--id", article_node, "--parent", ROOT_ID,
               "--tag", "article", "--prop", f"f.url={article['url']}",
               "--prop", f"f.reader={base}/#/a/{article['id']}")
            remember(done, article_node)
            added_articles += 1
        highlight_node = f"mg-h-{h['id']}"
        if exists(highlight_node):
            remember(done, highlight_node)
            continue
        props = ["--prop", f"f.url={article['url']}", "--prop", f"f.reader={base}/#/a/{article['id']}",
                 "--prop", f"f.highlighted={h['created_at'][:10]}"]
        if h.get("note"):
            props += ["--prop", f"f.note={h['note']}"]
        kb("add", h["text"], "--id", highlight_node, "--parent", article_node, "--tag", "highlight", *props)
        remember(done, highlight_node)
        added_highlights += 1

    print(f"marginalia-kb-sync: +{added_articles} articles, +{added_highlights} highlights "
          f"({len(highlights)} highlights in Marginalia) -> {KB_ROOT}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:  # one line in the journal, non-zero exit for systemd
        print(f"marginalia-kb-sync: {e}", file=sys.stderr)
        sys.exit(1)

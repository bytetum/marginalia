---
name: marginalia
description: Discuss, search and extend the user's personal reading library (saved articles, highlights, analyses and briefs) through the Marginalia MCP tools. Use when the user asks about what they have read or saved, wants a question answered from their library, or wants a highlight, article or synthesis stored.
---

# Marginalia

Marginalia is the user's personal reading library: articles they saved, passages they highlighted, and synthesis
products (analyses of one article, briefs across the library). You reach it through the `marginalia` MCP server.

## How to work

1. **Search first.** For any question about the user's reading, call `search` with a few keywords. Try synonyms
   and a second query if the first returns little. Use `list_articles` to browse when the question is broad.
2. **Read before you claim.** Call `get_article` on the relevant hits. A search snippet is not enough to
   summarise or quote an article. Call `list_highlights` (with `article_id`) to see what the user themself marked.
3. **Cite titles.** Name the article title (and URL when useful) for every claim that comes from the library.
   Quote verbatim; do not paraphrase inside quotation marks.
4. **Never invent articles.** If the library has nothing relevant, say so plainly. Do not fill gaps with made-up
   titles, quotes or highlights. Mark anything from your own background knowledge as such.
5. **Check existing products.** `list_syntheses` / `get_synthesis` may already hold an analysis or brief on the
   topic; reuse it and say it is an earlier product.

## Writing back

- `add_highlight(article_id, text, note?)`: record a passage the user wants to keep. Verbatim text only.
- `save_url(url)` / `save_markdown(url, title, markdown)`: add an article the user asks you to save.
- `save_synthesis(kind, title, body, direction?, article_ids)`: store a product worth keeping, such as an
  analysis you wrote for the user or a brief across several articles. `kind` is `analysis` (one article) or `brief`
  (many). Put the user's steer in `direction` and every article you relied on in `article_ids`. Only save when the
  result is finished and the user would want it again; do not save chit-chat.
- `analyse_article(article_id, direction?)` and `write_brief(direction?)` run Marginalia's own built-in agent
  (needs an LLM configured on the server and can take a minute). Prefer writing the analysis yourself and storing
  it with `save_synthesis` unless the user asks for the built-in one.

There are no delete tools by design.

## Connecting

Streamable HTTP (server started with `marginalia serve`, MCP at `/mcp`):

```json
{
  "mcpServers": {
    "marginalia": {
      "type": "http",
      "url": "http://100.103.153.13:7878/mcp",
      "headers": { "Authorization": "Bearer <MARGINALIA_TOKEN>" }
    }
  }
}
```

stdio (the harness spawns it; opens the database directly, so stop `marginalia serve` first, or point
`--data-dir` at a different directory):

```json
{
  "mcpServers": {
    "marginalia": {
      "command": "marginalia",
      "args": ["mcp"],
      "env": { "MARGINALIA_DATA": "/home/<user>/.local/share/marginalia/data" }
    }
  }
}
```

Config key names vary by harness (`mcpServers`, `mcp`, `[mcp_servers.marginalia]` in Codex TOML); the URL, header and
command above are what matter.

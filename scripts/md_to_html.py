#!/usr/bin/env python3
"""Convert the benchmark markdown report into a self-contained HTML file.

No external dependencies (stdlib only). Handles the small subset of Markdown
used by results/benchmark-report.md: ATX headers, tables, fenced code blocks,
blockquotes, horizontal rules, unordered/numbered lists, and inline code/bold/
italic. All CSS is embedded in a <style> block so the output renders offline.

Usage: python3 md2html.py <in.md> <out.html>
"""
import html
import re
import sys

CSS = """
:root {
  --bg: #f6f8fa;
  --card: #ffffff;
  --text: #1f2328;
  --muted: #57606a;
  --accent: #0969da;
  --accent-dark: #0b5394;
  --border: #d0d7de;
  --code-bg: #f2f4f6;
  --table-head: #eef1f4;
  --row-alt: #f8fafc;
  --good: #1a7f37;
  --warn: #9a6700;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica,
    Arial, "Noto Sans", sans-serif;
  line-height: 1.6;
  font-size: 16px;
}
.wrap {
  max-width: 920px;
  margin: 0 auto;
  padding: 40px 24px 80px;
}
header.top {
  border-bottom: 3px solid var(--accent);
  padding-bottom: 16px;
  margin-bottom: 8px;
}
h1 {
  font-size: 1.9em;
  margin: 0 0 8px;
  color: var(--accent-dark);
  line-height: 1.25;
}
h2 {
  font-size: 1.35em;
  margin-top: 36px;
  padding-bottom: 6px;
  border-bottom: 1px solid var(--border);
  color: var(--accent-dark);
}
h3 { font-size: 1.1em; margin-top: 24px; color: var(--accent-dark); }
p { margin: 12px 0; }
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
code {
  background: var(--code-bg);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 0.1em 0.35em;
  font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace;
  font-size: 0.9em;
}
pre {
  background: var(--code-bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 14px 16px;
  overflow-x: auto;
}
pre code { background: none; border: none; padding: 0; }
table {
  border-collapse: collapse;
  width: 100%;
  margin: 16px 0;
  font-size: 0.95em;
  box-shadow: 0 1px 2px rgba(31, 35, 40, 0.08);
}
th, td {
  border: 1px solid var(--border);
  padding: 8px 12px;
  text-align: left;
  vertical-align: top;
}
thead th {
  background: var(--table-head);
  font-weight: 600;
}
tbody tr:nth-child(even) { background: var(--row-alt); }
tbody tr:hover { background: #f0f6ff; }
blockquote {
  margin: 16px 0;
  padding: 10px 16px;
  border-left: 4px solid var(--accent);
  background: #f0f6ff;
  color: #0a3069;
  border-radius: 0 6px 6px 0;
}
blockquote code { background: #dbeafe; border-color: #bfdbfe; }
hr {
  border: none;
  border-top: 2px solid var(--border);
  margin: 28px 0;
}
ul, ol { margin: 12px 0; padding-left: 28px; }
li { margin: 6px 0; }
.meta { color: var(--muted); font-size: 0.95em; }
.tag {
  display: inline-block;
  font-size: 0.75em;
  font-weight: 600;
  padding: 2px 8px;
  border-radius: 10px;
  vertical-align: middle;
}
.tag-good { background: #dafbe1; color: var(--good); }
.tag-warn { background: #fff8c5; color: var(--warn); }
footer {
  margin-top: 48px;
  padding-top: 16px;
  border-top: 1px solid var(--border);
  color: var(--muted);
  font-size: 0.85em;
}
"""


def inline(text):
    """Apply HTML escaping then markdown inline formatting (code/bold/italic)."""
    t = html.escape(text)
    t = re.sub(r"`([^`]+)`", r"<code>\1</code>", t)
    t = re.sub(r"\*\*(.+?)\*\*", r"<strong>\1</strong>", t)
    t = re.sub(r"(?<!\*)\*([^*]+)\*(?!\*)", r"<em>\1</em>", t)
    return t


def parse_table(lines, i):
    """Parse a markdown table starting at header line i; returns (html, next_i)."""
    header = lines[i].strip()
    header = header.strip("|")
    cols = [c.strip() for c in header.split("|")]
    j = i + 1
    if j >= len(lines):
        return "", i
    # delimiter row (skip)
    j += 1
    rows = []
    while j < len(lines) and "|" in lines[j]:
        cells = [c.strip() for c in lines[j].strip().strip("|").split("|")]
        rows.append(cells)
        j += 1
    thead = "".join(f"<th>{inline(c)}</th>" for c in cols)
    tbody = ""
    for r in rows:
        tds = ""
        for idx, c in enumerate(r):
            td_class = ' style="text-align:center"' if idx == 0 else ""
            tds += f"<td{td_class}>{inline(c)}</td>"
        tbody += f"<tr>{tds}</tr>"
    return f"<table><thead><tr>{thead}</tr></thead><tbody>{tbody}</tbody></table>", j


def convert(md):
    lines = md.split("\n")
    out = []
    i = 0
    n = len(lines)
    while i < n:
        line = lines[i]
        stripped = line.strip()

        # Fenced code block.
        if stripped.startswith("```"):
            i += 1
            buf = []
            while i < n and not lines[i].strip().startswith("```"):
                buf.append(lines[i])
                i += 1
            i += 1  # closing fence
            out.append("<pre><code>" + html.escape("\n".join(buf)) + "</code></pre>")
            continue

        # Blank line.
        if not stripped:
            i += 1
            continue

        # Horizontal rule (--- not followed by a table context; standalone).
        if re.fullmatch(r"-{3,}|\*{3,}|_{3,}", stripped):
            out.append("<hr>")
            i += 1
            continue

        # Table: current line has | and next line is a delimiter row.
        if "|" in line and i + 1 < n:
            nxt = lines[i + 1].strip()
            if re.fullmatch(r"\|?[\s:|-]+\|?", nxt):
                html_tbl, i = parse_table(lines, i)
                out.append(html_tbl)
                continue

        # Headers.
        m = re.match(r"^(#{1,6})\s+(.*)$", stripped)
        if m:
            level = len(m.group(1))
            text = inline(m.group(2).strip())
            if level == 1:
                out.append(
                    '<header class="top"><h1>' + text + "</h1></header>"
                )
            else:
                out.append(f"<h{level}>{text}</h{level}>")
            i += 1
            continue

        # Blockquote (may span multiple lines).
        if stripped.startswith(">"):
            buf = []
            while i < n and lines[i].strip().startswith(">"):
                buf.append(lines[i].strip().lstrip(">").strip())
                i += 1
            out.append("<blockquote><p>" + inline(" ".join(buf)) + "</p></blockquote>")
            continue

        # Unordered list.
        if re.match(r"^[-*+]\s+", stripped):
            buf = []
            while i < n and re.match(r"^[-*+]\s+", lines[i].strip()):
                buf.append(inline(lines[i].strip()[2:]))
                i += 1
            out.append("<ul>" + "".join(f"<li>{x}</li>" for x in buf) + "</ul>")
            continue

        # Ordered list.
        if re.match(r"^\d+\.\s+", stripped):
            buf = []
            while i < n and re.match(r"^\d+\.\s+", lines[i].strip()):
                buf.append(inline(re.sub(r"^\d+\.\s+", "", lines[i].strip())))
                i += 1
            out.append("<ol>" + "".join(f"<li>{x}</li>" for x in buf) + "</ol>")
            continue

        # Paragraph: gather consecutive non-blank, non-special lines.
        buf = []
        while i < n:
            s = lines[i].strip()
            if not s:
                break
            if s.startswith("```") or s.startswith("#") or re.match(
                r"^[-*+]\s+", s
            ) or re.match(r"^\d+\.\s+", s) or s.startswith(">"):
                break
            if "|" in s and i + 1 < n and re.fullmatch(
                r"\|?[\s:|-]+\|?", lines[i + 1].strip()
            ):
                break
            if re.fullmatch(r"-{3,}|\*{3,}|_{3,}", s):
                break
            buf.append(s)
            i += 1
        if buf:
            out.append("<p>" + inline(" ".join(buf)) + "</p>")
            continue

        # Fallback.
        out.append("<p>" + inline(stripped) + "</p>")
        i += 1

    return "\n".join(out)


def main():
    in_path, out_path = sys.argv[1], sys.argv[2]
    with open(in_path, encoding="utf-8") as f:
        md = f.read()
    body = convert(md)
    page = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>LibzipInRust — Phase 5 Benchmark Report</title>
<style>{CSS}</style>
</head>
<body>
<div class="wrap">
{body}
<footer>
<p>LibzipInRust — Phase 5 benchmark report. Self-contained HTML generated from
<code>results/benchmark-report.md</code>. Raw data in <code>results/benchmark-*.csv</code>.</p>
</footer>
</div>
</body>
</html>
"""
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(page)
    print(f"wrote {out_path}")


if __name__ == "__main__":
    main()

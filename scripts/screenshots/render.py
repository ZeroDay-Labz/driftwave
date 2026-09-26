#!/usr/bin/env python3
"""Render `tmux capture-pane -e -p` output (ANSI, truecolor) to a PNG that
looks like a terminal window.

    render.py out.png pane.ans [--title driftwave] [pane2.ans --title "lyrics"]...

Several panes are placed side by side, each in its own window frame.
Needs Pillow. Fonts: JetBrainsMono Nerd Font Mono, with Noto/Symbola fallbacks.
"""

import os
import re
import sys
import unicodedata
from glob import glob

from PIL import Image, ImageDraw, ImageFont

SCALE = 2
FONT_PX = 14 * SCALE
BG = (22, 21, 30)
FG = (225, 225, 232)
FRAME_BG = (14, 13, 20)
TITLE_FG = (150, 150, 165)

FONT_DIRS = [os.path.expanduser("~/.local/share/fonts"), "/usr/share/fonts", "/usr/local/share/fonts"]


def find_font(pattern):
    for d in FONT_DIRS:
        hits = sorted(glob(os.path.join(d, "**", pattern), recursive=True))
        if hits:
            return hits[0]
    return None


MAIN = {
    (False, False): find_font("JetBrainsMonoNerdFontMono-Regular.ttf"),
    (True, False): find_font("JetBrainsMonoNerdFontMono-Bold.ttf"),
    (False, True): find_font("JetBrainsMonoNerdFontMono-Italic.ttf"),
    (True, True): find_font("JetBrainsMonoNerdFontMono-BoldItalic.ttf"),
}
if not MAIN[(False, False)]:
    MAIN = {k: find_font("NotoSansMono-Regular.ttf") for k in MAIN}
FALLBACKS = [
    p
    for p in (
        find_font("NotoSansSymbols2-Regular.ttf"),
        find_font("Symbola.ttf"),
        find_font("NotoSansMath-Regular.ttf"),
        find_font("NotoSansSymbols*.ttf"),
        find_font("NotoSansMono-Regular.ttf"),
    )
    if p
]

_fonts = {}


def font(path):
    if path not in _fonts:
        _fonts[path] = ImageFont.truetype(path, FONT_PX)
    return _fonts[path]


_notdef = {}


def has_glyph(path, ch):
    """True unless the font would draw its 'missing glyph' box for ch."""
    f = font(path)
    if path not in _notdef:
        m = f.getmask("\U000FFFFD")
        _notdef[path] = (m.size, bytes(m))
    m = f.getmask(ch)
    return (m.size, bytes(m)) != _notdef[path]


_pick = {}


def font_for(ch, bold, italic):
    key = (ch, bold, italic)
    if key not in _pick:
        main = MAIN[(bold, italic)] or MAIN[(False, False)]
        choice = main
        if not has_glyph(main, ch):
            choice = next((p for p in FALLBACKS if has_glyph(p, ch)), main)
        _pick[key] = choice
    return font(_pick[key])


REF = font(MAIN[(False, False)])
CELL_W = round(REF.getlength("M"))
ASC, DESC = REF.getmetrics()
CELL_H = round((ASC + DESC) * 1.12)
BASELINE = round((CELL_H - (ASC + DESC)) / 2 + ASC)

ANSI_16 = [
    (30, 30, 40), (230, 90, 90), (140, 210, 110), (240, 200, 100),
    (100, 160, 240), (200, 120, 230), (90, 200, 210), (210, 210, 220),
    (100, 100, 115), (255, 120, 120), (170, 235, 140), (255, 225, 130),
    (130, 185, 255), (225, 150, 255), (130, 225, 235), (245, 245, 250),
]


def xterm256(n):
    if n < 16:
        return ANSI_16[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return (steps[n // 36], steps[(n // 6) % 6], steps[n % 6])
    v = 8 + (n - 232) * 10
    return (v, v, v)


SGR = re.compile(r"\x1b\[([0-9;:]*)m")
OTHER_ESC = re.compile(r"\x1b(\[[0-9;?]*[A-Za-z]|\][^\x07]*\x07|[()][0-9A-Za-z])")


def char_width(ch):
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


def parse(text):
    """Grid of rows; each cell (char, fg, bg, bold, italic, underline, strike)."""
    rows = []
    st = dict(fg=None, bg=None, bold=False, dim=False, italic=False, underline=False, reverse=False, strike=False)
    for line in text.split("\n"):
        row = []
        pos = 0
        for m in re.finditer(r"\x1b\[[0-9;:]*m|\x1b(?:\[[0-9;?]*[A-Za-z]|\][^\x07]*\x07|[()][0-9A-Za-z])", line + "\x1b[m"[:0]):
            chunk = line[pos:m.start()]
            emit(row, chunk, st)
            pos = m.end()
            sgr = SGR.fullmatch(m.group(0))
            if sgr:
                apply_sgr(st, sgr.group(1))
        emit(row, line[pos:], st)
        rows.append(row)
    while rows and not rows[-1]:
        rows.pop()
    return rows


def emit(row, chunk, st):
    for ch in chunk:
        w = char_width(ch)
        if w == 0:
            if row:
                row[-1] = (row[-1][0] + ch,) + row[-1][1:]
            continue
        fg, bg = st["fg"] or FG, st["bg"]
        if st["dim"]:
            fg = tuple(int(c * 0.6 + b * 0.4) for c, b in zip(fg, bg or BG))
        if st["reverse"]:
            fg, bg = (bg or BG), fg
        cell = (ch, fg, bg, st["bold"], st["italic"], st["underline"], st["strike"])
        row.append(cell)
        if w == 2:
            row.append(None)  # right half of a wide character


def apply_sgr(st, params):
    ps = [int(p) if p else 0 for p in params.replace(":", ";").split(";")] if params else [0]
    i = 0
    while i < len(ps):
        p = ps[i]
        if p == 0:
            st.update(fg=None, bg=None, bold=False, dim=False, italic=False, underline=False, reverse=False, strike=False)
        elif p == 1:
            st["bold"] = True
        elif p == 2:
            st["dim"] = True
        elif p == 3:
            st["italic"] = True
        elif p == 4:
            st["underline"] = True
        elif p == 7:
            st["reverse"] = True
        elif p == 9:
            st["strike"] = True
        elif p == 22:
            st["bold"] = st["dim"] = False
        elif p == 23:
            st["italic"] = False
        elif p == 24:
            st["underline"] = False
        elif p == 27:
            st["reverse"] = False
        elif p == 29:
            st["strike"] = False
        elif 30 <= p <= 37:
            st["fg"] = ANSI_16[p - 30]
        elif 90 <= p <= 97:
            st["fg"] = ANSI_16[p - 90 + 8]
        elif 40 <= p <= 47:
            st["bg"] = ANSI_16[p - 40]
        elif 100 <= p <= 107:
            st["bg"] = ANSI_16[p - 100 + 8]
        elif p == 39:
            st["fg"] = None
        elif p == 49:
            st["bg"] = None
        elif p in (38, 48) and i + 1 < len(ps):
            key = "fg" if p == 38 else "bg"
            if ps[i + 1] == 2 and i + 4 < len(ps):
                st[key] = tuple(ps[i + 2:i + 5])
                i += 4
            elif ps[i + 1] == 5 and i + 2 < len(ps):
                st[key] = xterm256(ps[i + 2])
                i += 2
        i += 1


def draw_block(d, ch, x, y, fg):
    """Draw block elements as exact rectangles so they tile seamlessly."""
    cp = ord(ch)
    w, h = CELL_W, CELL_H
    if ch == "█":
        d.rectangle([x, y, x + w - 1, y + h - 1], fill=fg)
    elif ch == "▀":
        d.rectangle([x, y, x + w - 1, y + h // 2 - 1], fill=fg)
    elif 0x2581 <= cp <= 0x2587:  # ▁..▇ lower eighths
        eighths = cp - 0x2580
        top = y + h - round(h * eighths / 8)
        d.rectangle([x, top, x + w - 1, y + h - 1], fill=fg)
    elif ch == "▔":
        d.rectangle([x, y, x + w - 1, y + max(1, h // 8) - 1], fill=fg)
    elif ch == "▌":
        d.rectangle([x, y, x + w // 2 - 1, y + h - 1], fill=fg)
    elif ch == "▐":
        d.rectangle([x + w // 2, y, x + w - 1, y + h - 1], fill=fg)
    elif 0x2589 <= cp <= 0x258F:  # ▉..▏ left eighths
        eighths = 0x2590 - cp
        d.rectangle([x, y, x + round(w * eighths / 8) - 1, y + h - 1], fill=fg)
    else:
        return False
    return True


def draw_box(d, ch, x, y, fg):
    """Box-drawing lines and rounded corners, drawn to join across cells."""
    w, h = CELL_W, CELL_H
    cx, cy = x + w // 2, y + h // 2
    t = max(1, SCALE)
    heavy = ch in "━┃"
    lw = t * 2 if heavy else t
    horiz = {"─": (x, x + w), "━": (x, x + w), "╭": (cx, x + w), "╰": (cx, x + w), "╮": (x, cx), "╯": (x, cx)}
    vert = {"│": (y, y + h), "┃": (y, y + h), "╭": (cy, y + h), "╮": (cy, y + h), "╰": (y, cy), "╯": (y, cy)}
    if ch not in horiz and ch not in vert:
        return False
    r = min(w, h) // 2
    if ch in "╭╮╰╯":
        box = {
            "╭": [cx, cy, cx + 2 * r, cy + 2 * r],
            "╮": [cx - 2 * r, cy, cx, cy + 2 * r],
            "╰": [cx, cy - 2 * r, cx + 2 * r, cy],
            "╯": [cx - 2 * r, cy - 2 * r, cx, cy],
        }[ch]
        start = {"╭": 180, "╮": 270, "╰": 90, "╯": 0}[ch]
        d.arc(box, start, start + 90, fill=fg, width=lw)
        if ch in "╭╰":
            d.line([cx + r, cy, x + w, cy], fill=fg, width=lw)
        else:
            d.line([x, cy, cx - r, cy], fill=fg, width=lw)
        if ch in "╭╮":
            d.line([cx, cy + r, cx, y + h], fill=fg, width=lw)
        else:
            d.line([cx, y, cx, cy - r], fill=fg, width=lw)
        return True
    if ch in horiz:
        x0, x1 = horiz[ch]
        d.line([x0, cy, x1, cy], fill=fg, width=lw)
    if ch in vert:
        y0, y1 = vert[ch]
        d.line([cx, y0, cx, y1], fill=fg, width=lw)
    return True


def render_pane(rows):
    cols = max((len(r) for r in rows), default=1)
    img = Image.new("RGB", (cols * CELL_W, len(rows) * CELL_H), BG)
    d = ImageDraw.Draw(img)
    for ry, row in enumerate(rows):
        for cx, cell in enumerate(row):
            if cell is None:
                continue
            ch, fg, bg, bold, italic, underline, strike = cell
            x, y = cx * CELL_W, ry * CELL_H
            wide = cx + 1 < len(row) and row[cx + 1] is None
            cw = CELL_W * (2 if wide else 1)
            if bg:
                d.rectangle([x, y, x + cw - 1, y + CELL_H - 1], fill=bg)
            if ch == " ":
                pass
            elif draw_block(d, ch, x, y, fg) or draw_box(d, ch, x, y, fg):
                pass
            else:
                f = font_for(ch[0], bold, italic)
                # Center narrow fallback glyphs in their cell.
                gw = f.getlength(ch)
                gx = x + max(0, (cw - gw) / 2) if gw < cw * 0.8 or gw > cw else x
                d.text((gx, y + BASELINE), ch, font=f, fill=fg, anchor="ls")
            if underline:
                d.line([x, y + CELL_H - SCALE * 2, x + cw, y + CELL_H - SCALE * 2], fill=fg, width=SCALE)
            if strike:
                d.line([x, y + CELL_H // 2, x + cw, y + CELL_H // 2], fill=fg, width=SCALE)
    return img


def framed(pane, title):
    pad, bar = 14 * SCALE, 30 * SCALE
    w, h = pane.width + pad * 2, pane.height + pad + bar
    win = Image.new("RGB", (w, h), FRAME_BG)
    d = ImageDraw.Draw(win)
    d.rounded_rectangle([0, 0, w - 1, h - 1], radius=10 * SCALE, fill=BG)
    # Title bar: three dots and a centered title, like a terminal window.
    for i, color in enumerate([(45, 190, 190), (60, 120, 230), (230, 60, 130)]):
        cx, cy, r = w - pad - i * 18 * SCALE - 6 * SCALE, bar // 2, 6 * SCALE
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=color)
    tf = font(MAIN[(False, False)])
    d.text((w / 2, bar / 2), title, font=tf, fill=TITLE_FG, anchor="mm")
    win.paste(pane, (pad, bar))
    return win


def main(argv):
    if len(argv) < 3:
        sys.exit(__doc__)
    out, rest = argv[1], argv[2:]
    panes, i = [], 0
    while i < len(rest):
        path, title = rest[i], "driftwave"
        if i + 2 < len(rest) + 1 and i + 1 < len(rest) and rest[i + 1] == "--title":
            title = rest[i + 2]
            i += 2
        i += 1
        with open(path, encoding="utf-8") as fh:
            panes.append(framed(render_pane(parse(fh.read())), title))
    gap, margin = 24 * SCALE, 28 * SCALE
    width = sum(p.width for p in panes) + gap * (len(panes) - 1) + margin * 2
    height = max(p.height for p in panes) + margin * 2
    canvas = Image.new("RGB", (width, height), FRAME_BG)
    x = margin
    for p in panes:
        canvas.paste(p, (x, margin + (height - margin * 2 - p.height) // 2))
        x += p.width + gap
    # Rounded window corners: cut against the page background.
    canvas.save(out, optimize=True)
    print(f"{out}: {canvas.width}x{canvas.height}")


if __name__ == "__main__":
    main(sys.argv)

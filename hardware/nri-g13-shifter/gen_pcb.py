#!/usr/bin/env python3
"""Разводка платы переходника NRI G-13 <-> RP2350-Plus.

Замысел разводки — ортогональный, и он же причина, по которой её удалось
сгенерировать, а не рисовать мышью:

* **верх (F.Cu)** — сигналы, горизонтальными строками, по строке на канал;
* **низ (B.Cu)** — питание, вертикальными шинами, пересекающими все строки.

Пересечений между ними не бывает по построению, а семь строк отличаются
только координатой y.

Цепи берутся из нетлиста схемы, а не задаются здесь заново: разойтись они не
могут в принципе.

Запуск: python3 gen_pcb.py && kicad-cli pcb drc nri-g13-shifter.kicad_pcb
"""

import hashlib
import re

from gen_sch import tokenize, parse, head, find_all
from fp import load_fp, fp_pads

PROJECT = "nri-g13-shifter"
TRACE = 0.3          # минимум по умолчанию в KiCad — 0.254 мм
CLEAR = 0.2
VIA_D, VIA_DRILL = 1.0, 0.6   # минимумы KiCad: 0.889 и 0.508

_n = 0


def uid(tag=""):
    global _n
    _n += 1
    h = hashlib.sha1(f"pcb:{tag}:{_n}".encode()).hexdigest()
    return f"{h[0:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:32]}"


# --- Цепи из нетлиста схемы ----------------------------------------------

def read_netlist(path="net.txt"):
    s = open(path).read()
    blocks = re.split(r"\n\t\t\(net\n", s[s.index("(nets"):])
    pad_net, names = {}, []
    for b in blocks[1:]:
        name = re.search(r'\(name "([^"]+)"\)', b).group(1)
        names.append(name)
        for ref, pin in re.findall(r'\(ref "([^"]+)"\)\s*\n\s*\(pin "([^"]+)"\)', b):
            pad_net[(ref, pin)] = name
    return pad_net, names


def read_components(path="net.txt"):
    """Обозначение -> номинал, посадочное место, признак «не ставить».

    Всё берётся из нетлиста, а не задаётся здесь: разойтись со схемой не может.
    """
    s = open(path).read()
    body = s[s.index("(components"):s.index("(libparts")]
    out = {}
    for blk in re.split(r"\n\t\t\(comp\n", body)[1:]:
        ref = re.search(r'\(ref "([^"]+)"\)', blk).group(1)
        val = re.search(r'\(value "([^"]*)"\)', blk).group(1)
        fp = re.search(r'\(footprint "([^"]+)"\)', blk).group(1)
        dnp = '(name "dnp")' in blk
        out[ref] = (val, fp, dnp)
    return out


PAD_NET, NET_NAMES = read_netlist()
COMP = read_components()
NET_ID = {"": 0}
for i, n in enumerate(sorted(NET_NAMES), start=1):
    NET_ID[n] = i


def net_of(ref, pad):
    return PAD_NET.get((ref, pad), "")


# --- Геометрия ------------------------------------------------------------

ROWS = [24 + i * 10 for i in range(7)]      # центр строки канала
CH = [("GP3", "LINE1"), ("GP4", "LINE2"), ("GP5", "LINE3"), ("GP6", "LINE4"),
      ("GP7", "LINE5"), ("GP8", "LINE6"), ("GP9", "INHIBIT")]

X_J2, X_LED, X_RLED = 6.0, 20.0, 26.0
X_GND, X_V3A, X_V3B, X_V12 = 32.0, 36.0, 48.0, 70.0
X_R3V3, X_Q, X_R12 = 42.0, 54.0, 64.0
X_JOG_IN, X_CHAN = 57.0, 76.0
X_J1 = 88.0

Y_J2 = 42.57          # верхний вывод гребёнки
Y_J3 = 12.0
BOARD = (0.0, 5.0, 96.0, 97.0)


out = []
pads_abs = {}      # (ref, pad) -> (x, y)


def emit(s):
    out.append(s)


def _blocks(text, opener):
    """Найти все сбалансированные блоки, начинающиеся с `opener`."""
    res, i = [], 0
    while True:
        i = text.find(opener, i)
        if i < 0:
            return res
        d = 0
        for j in range(i, len(text)):
            if text[j] == "(":
                d += 1
            elif text[j] == ")":
                d -= 1
                if d == 0:
                    break
        res.append((i, j + 1))
        i = j + 1


def place_fp(ref, x, y, rot=0):
    """Поставить посадочное место и запомнить координаты его площадок.

    Поворот только на 0 или 180: этого хватает всей плате, а значит не нужно
    возиться с матрицей поворота, где легко ошибиться незаметно.
    """
    assert rot in (0, 180)
    value, fp, dnp = COMP[ref]
    lib, name = fp.split(":")
    text, tree = load_fp(lib, name)
    pads = fp_pads(tree)

    sign = 1 if rot == 0 else -1
    for num, (px, py, _, _) in pads.items():
        pads_abs[(ref, num)] = (round(x + sign * px, 4), round(y + sign * py, 4))

    # Заголовок: имя с библиотекой, слой, положение.
    at = f"(at {x} {y})" if rot == 0 else f"(at {x} {y} {rot})"
    text = text.replace(f'(footprint "{name}"',
                        f'(footprint "{lib}:{name}"\n\t(uuid "{uid("f")}")\n\t{at}', 1)
    # Поворот KiCad записывает не только в заголовок, но и в каждый вложенный
    # `at`. Без этого плата работает, но сверка с библиотекой считает такое
    # посадочное место изменённым и сыплет предупреждениями.
    if rot:
        head_at = text.index(at)
        tail = text[head_at + len(at):]

        def turn(m):
            a = (float(m.group(3) or 0) + rot) % 360
            return f"(at {m.group(1)} {m.group(2)} {a:g})"

        tail = re.sub(r"\(at (-?[\d.]+) (-?[\d.]+)(?: (-?[\d.]+))?\)", turn, tail)
        text = text[:head_at + len(at)] + tail

    # Служебные поля .kicad_mod, которых в плате быть не должно.
    for key in ('(version ', '(generator ', '(generator_version '):
        for s, e in reversed(_blocks(text, "\n\t" + key)):
            text = text[:s] + text[e:]

    # Обозначение и номинал.
    text = text.replace('"Reference" "REF**"', f'"Reference" "{ref}"', 1)
    text = re.sub(r'\(property "Value" "[^"]*"', f'(property "Value" "{value}"',
                  text, count=1)
    # «Не ставить» должно совпадать со схемой, иначе сверка ругается, а на
    # производство уедет лишняя деталь.
    if dnp:
        text = re.sub(r"\(attr ([a-z_ ]+)\)", r"(attr \1 dnp)", text, count=1)

    # Цепи в площадки.
    for s, e in reversed(_blocks(text, '(pad "')):
        num = re.match(r'\(pad "([^"]+)"', text[s:e]).group(1)
        net = net_of(ref, num)
        if not net:
            continue
        text = text[:e - 1] + f'\n\t\t(net {NET_ID[net]} "{net}")\n\t' + text[e - 1:]

    emit("\t" + text.replace("\n", "\n\t"))


def seg(x1, y1, x2, y2, net, layer="F.Cu", width=TRACE):
    if (x1, y1) == (x2, y2):
        return
    emit(f'\t(segment\n\t\t(start {round(x1,4)} {round(y1,4)}) '
         f'(end {round(x2,4)} {round(y2,4)})\n\t\t(width {width}) '
         f'(layer "{layer}") (net {NET_ID[net]})\n\t\t(uuid "{uid("s")}")\n\t)')


def path(points, net, layer="F.Cu"):
    """Ломаная из точек — как рисуют дорожку руками."""
    for (x1, y1), (x2, y2) in zip(points, points[1:]):
        seg(x1, y1, x2, y2, net, layer)


def via(x, y, net):
    emit(f'\t(via\n\t\t(at {round(x,4)} {round(y,4)}) (size {VIA_D}) '
         f'(drill {VIA_DRILL})\n\t\t(layers "F.Cu" "B.Cu") (net {NET_ID[net]})'
         f'\n\t\t(uuid "{uid("v")}")\n\t)')


def edge(x1, y1, x2, y2):
    emit(f'\t(gr_line\n\t\t(start {x1} {y1}) (end {x2} {y2})\n\t\t'
         f'(stroke (width 0.1) (type default)) (layer "Edge.Cuts")'
         f'\n\t\t(uuid "{uid("e")}")\n\t)')


def note(s, x, y, size=1.5, layer="F.SilkS"):
    emit(f'\t(gr_text "{s}"\n\t\t(at {x} {y})\n\t\t(layer "{layer}")'
         f'\n\t\t(uuid "{uid("gt")}")\n\t\t(effects (font (size {size} {size}) '
         f'(thickness {size/7:.2f})) (justify left bottom))\n\t)')


def P(ref, pad):
    return pads_abs[(ref, pad)]


# --- Размещение и разводка ------------------------------------------------

N3, N12, NG = "+3V3", "+12V", "GND"

place_fp("J1", X_J1, 44.92)
place_fp("J2", X_J2, Y_J2)
place_fp("J3", X_J2, Y_J3)

# Шины питания: вертикальные, по низу. Сигналы идут горизонтально по верху,
# поэтому пересечься они не могут в принципе.
RAILS = {N3: [(X_V3A, 17, 88), (X_V3B, 17, 86.7)],
         N12: [(X_V12, 9, 90)],
         NG: [(X_GND, 12, 93)]}
for net, runs in RAILS.items():
    for x, y1, y2 in runs:
        seg(x, y1, x, y2, net, "B.Cu", 0.5)
# Две шины 3.3 В соединяются поверху, где строк ещё нет.
seg(X_V3A, 17, X_V3B, 17, N3, "B.Cu", 0.5)

for i, yc in enumerate(ROWS):
    gp, line = "/" + CH[i][0], "/" + CH[i][1]
    q, r3, r12 = f"Q{i+1}", f"R{i+1}", f"R{i+11}"
    place_fp(q, X_Q, yc)
    # Поворот на 180: у резисторов вывод 1 сидит на питании, а шина питания
    # по разводке всегда с противоположной стороны от сигнала.
    place_fp(r3, X_R3V3, yc + 2.7, rot=180)
    place_fp(r12, X_R12, yc, rot=180)

    y_gp = yc + 0.95                      # исток BSS138 и вся линия к плате
    path([(17, y_gp), P(q, "2")], gp)     # магистраль канала
    path([(39, y_gp), (39, yc + 2.7), P(r3, "2")], gp)
    path([P(r3, "1"), (X_V3B, yc + 2.7)], N3)
    via(X_V3B, yc + 2.7, N3)
    path([P(q, "1"), (X_V3B, yc - 0.95)], N3)   # затвор на 3.3 В
    via(X_V3B, yc - 0.95, N3)

    path([P(q, "3"), P(r12, "2")], line)        # сток на сторону приёмника
    path([P(r12, "1"), (X_V12, yc)], N12)
    via(X_V12, yc, N12)
    # Отвод к разъёму обходит R12 сверху: в линию его не поставить, там уже
    # сидит сток.
    path([(X_JOG_IN, yc), (X_JOG_IN, yc - 3.2), (X_CHAN, yc - 3.2)], line)

    if i < 6:
        led, rled = f"D{i+1}", f"R{i+21}"
        place_fp(led, X_LED, yc + 5)
        place_fp(rled, X_RLED, yc + 5, rot=180)
        path([(17, y_gp), (17, yc + 5), P(led, "1")], gp)
        path([P(led, "2"), P(rled, "2")], net_of(led, "2"))
        path([P(rled, "1"), (X_V3A, yc + 5)], N3)
        via(X_V3A, yc + 5, N3)

# Разбор гребёнки платы. Выводы и строки идут в одном порядке, поэтому веер
# получается непересекающимся сам собой.
for i, yc in enumerate(ROWS):
    px, py = P("J2", str(i + 3))
    path([(px, py), (8, py), (16, yc + 0.95), (17, yc + 0.95)], "/" + CH[i][0])

path([P("J2", "1"), (3, 42.57), (3, 20), (X_V3A, 20)], N3)
via(X_V3A, 20, N3)
for pin in ("2", "10"):
    path([P("J2", pin), (X_GND, P("J2", pin)[1])], NG, "B.Cu")

path([P("J3", "1"), (6, 9), (X_V12, 9)], N12)
via(X_V12, 9, N12)
path([P("J3", "2"), (X_GND, Y_J3)], NG, "B.Cu")

# Разбор разъёма приёмника. Порядок контактов там свой, и в строки он ложится
# с перехлёстом, поэтому половина уходит на нижний слой: внутри каждой
# половины порядок уже монотонный и пересечений нет.
UP = [("7", 84, 0), ("8", 82, 1), ("9", 80, 2), ("10", 78, 3)]
DOWN = [("3", 78, 4), ("4", 80, 5), ("6", 82, 6)]
MIDGAP = {"8": 53.81, "10": 56.35, "4": 48.73, "6": 51.27}

for pin, lane, row in UP:
    px, py = P("J1", pin)
    pts = [(px, py)]
    if pin in MIDGAP:
        pts += [(px, MIDGAP[pin])]
    y_jog = ROWS[row] - 3.2
    pts += [(lane, pts[-1][1]), (lane, y_jog), (X_CHAN, y_jog)]
    path(pts, "/" + CH[row][1])

for pin, lane, row in DOWN:
    px, py = P("J1", pin)
    pts = [(px, py)]
    if pin in MIDGAP:
        pts += [(px, MIDGAP[pin])]
    y_jog = ROWS[row] - 3.2
    pts += [(lane, pts[-1][1]), (lane, y_jog), (X_CHAN, y_jog)]
    path(pts, "/" + CH[row][1], "B.Cu")
    via(X_CHAN, y_jog, "/" + CH[row][1])

# Питание разъёма приёмника уводим поверх строк: там пусто.
path([P("J1", "1"), (X_J1, 16), (X_GND, 16)], NG)
via(X_GND, 16, NG)
path([P("J1", "2"), (90.54, 14), (X_V12, 14)], N12, "B.Cu")

# Развязка.
place_fp("C1", 34, 88, rot=180)
path([P("C1", "1"), (X_V3A, 88)], N3)
via(X_V3A, 88, N3)
path([P("C1", "2"), (X_GND, 88)], NG)
via(X_GND, 88, NG)

seg(X_GND, 93, 74.775, 93, NG)          # общая шина земли под строками
via(X_GND, 93, NG)
place_fp("C2", 66, 90, rot=180)
place_fp("C3", 74, 90)
path([P("C2", "1"), (X_V12, 90)], N12)
via(X_V12, 90, N12)
path([P("C2", "2"), (P("C2", "2")[0], 93)], NG)
path([P("C3", "1"), (X_V12, 90)], N12)
path([P("C3", "2"), (P("C3", "2")[0], 93)], NG)

# Контур платы и надписи.
x0, y0, x1, y1 = BOARD
for a, b, c, d in ((x0, y0, x1, y0), (x1, y0, x1, y1), (x1, y1, x0, y1), (x0, y1, x0, y0)):
    edge(a, b, c, d)
note("NRI G-13 <-> RP2350-Plus", 30, 8, 2.0)
note("R11..R16 - только если приёмник не тянет линии сам", 30, 96, 1.2)


# --- Запись ---------------------------------------------------------------

LAYERS = '''	(layers
		(0 "F.Cu" signal)
		(2 "B.Cu" signal)
		(9 "F.Adhes" user "F.Adhesive")
		(11 "B.Adhes" user "B.Adhesive")
		(13 "F.Paste" user)
		(15 "B.Paste" user)
		(5 "F.SilkS" user "F.Silkscreen")
		(7 "B.SilkS" user "B.Silkscreen")
		(1 "F.Mask" user)
		(3 "B.Mask" user)
		(17 "Dwgs.User" user "User.Drawings")
		(19 "Cmts.User" user "User.Comments")
		(21 "Eco1.User" user "User.Eco1")
		(23 "Eco2.User" user "User.Eco2")
		(25 "Edge.Cuts" user)
		(27 "Margin" user)
		(31 "F.CrtYd" user "F.Courtyard")
		(29 "B.CrtYd" user "B.Courtyard")
	)'''

nets = "\n".join(f'\t(net {i} "{n}")' for n, i in sorted(NET_ID.items(), key=lambda kv: kv[1]))

pcb = f'''(kicad_pcb
\t(version 20241229)
\t(generator "gen_pcb.py")
\t(generator_version "9.0")
\t(general
\t\t(thickness 1.6)
\t\t(legacy_teardrops no)
\t)
\t(paper "A4")
\t(title_block
\t\t(title "NRI G-13 <-> RP2350-Plus level shifter")
\t\t(rev "A")
\t)
{LAYERS}
\t(setup
\t\t(pad_to_mask_clearance 0)
\t\t(allow_soldermask_bridges_in_footprints no)
\t)
{nets}
{chr(10).join(out)}
)
'''

with open(f"{PROJECT}.kicad_pcb", "w") as f:
    f.write(pcb)

d = sum(1 if c == "(" else -1 if c == ")" else 0 for c in pcb)
print(f"посадочных мест: {len(set(r for r, _ in pads_abs))}, "
      f"объектов: {len(out)}, скобки {'сходятся' if not d else 'НЕТ ' + str(d)}")

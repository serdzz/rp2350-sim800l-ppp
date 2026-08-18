#!/usr/bin/env python3
"""Генератор схемы переходника NRI G-13 <-> 3.3 В GPIO.

Схема пишется скриптом, а не рисуется мышью, по двум причинам. Семь каналов
отличаются только именами цепей, и копировать их руками — верный способ
ошибиться в одном из семи. И соединения тут задаются списком цепей, а не
координатами: опечатка в имени видна в проверке внизу файла, а не на собранной
плате.

Определения символов берутся **из установленных библиотек KiCad** и
встраиваются в файл целиком. Поэтому распиновка BSS138 и разъёмов настоящая, а
не восстановленная по памяти, и схема при этом открывается на машине без этих
библиотек.

Координаты проводов вычисляются из положения выводов в самих символах, а не
подбираются на глаз.

Запуск: python3 gen_sch.py && kicad-cli sch erc nri-g13-shifter.kicad_sch
"""

import hashlib
import os
import re

PROJECT = "nri-g13-shifter"
SYMDIR = "/Applications/KiCad/KiCad.app/Contents/SharedSupport/symbols"

# --- Разбор s-выражений ---------------------------------------------------


def tokenize(s):
    out, i, n = [], 0, len(s)
    while i < n:
        c = s[i]
        if c in "()":
            out.append(c)
            i += 1
        elif c == '"':
            j = i + 1
            buf = []
            while s[j] != '"':
                if s[j] == "\\":
                    buf.append(s[j + 1])
                    j += 2
                else:
                    buf.append(s[j])
                    j += 1
            out.append(("str", "".join(buf)))
            i = j + 1
        elif c.isspace():
            i += 1
        else:
            j = i
            while j < n and not s[j].isspace() and s[j] not in "()":
                j += 1
            out.append(("atom", s[i:j]))
            i = j
    return out


def parse(tokens, pos=0):
    assert tokens[pos] == "("
    node, pos = [], pos + 1
    while tokens[pos] != ")":
        if tokens[pos] == "(":
            sub, pos = parse(tokens, pos)
            node.append(sub)
        else:
            node.append(tokens[pos])
            pos += 1
    return node, pos + 1


def head(node):
    return node[0][1] if isinstance(node[0], tuple) else None


def find_all(node, name):
    return [c for c in node if isinstance(c, list) and head(c) == name]


def raw_block(text, start):
    """Вырезать сбалансированный блок, начинающийся со `start`."""
    i = text.index(start)
    depth = 0
    for j in range(i, len(text)):
        if text[j] == "(":
            depth += 1
        elif text[j] == ")":
            depth -= 1
            if depth == 0:
                return text[i:j + 1]
    raise ValueError(start)


_libcache = {}


def load_symbol(libname, symname):
    """Достать символ из библиотеки: исходный текст и разобранное дерево."""
    if libname not in _libcache:
        with open(os.path.join(SYMDIR, f"{libname}.kicad_sym")) as f:
            _libcache[libname] = f.read()
    text = _libcache[libname]
    block = raw_block(text, f'(symbol "{symname}"\n')
    tree, _ = parse(tokenize(block))
    return block, tree


def symbol_pins(tree):
    """Выводы символа: номер -> (x, y) в системе координат символа.

    Выводы лежат не в корне, а в подсимволах вида `R_1_1`, поэтому обходим
    рекурсивно.
    """
    pins = {}

    def walk(node):
        for child in node:
            if not isinstance(child, list):
                continue
            if head(child) == "pin":
                at = find_all(child, "at")[0]
                num = find_all(child, "number")[0][1][1]
                pins[num] = (float(at[1][1]), float(at[2][1]))
            else:
                walk(child)

    walk(tree)
    return pins


def top_children(block):
    """Прямые потомки блока символа, как исходный текст."""
    i = block.index("\n")  # пропускаем строку с именем
    depth, start, out = 0, None, []
    for j in range(i, len(block)):
        c = block[j]
        if c == "(":
            if depth == 0:
                start = j
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                out.append(block[start:j + 1])
    return out


ATTR_KEYS = ("pin_numbers", "pin_names", "exclude_from_sim", "in_bom",
             "on_board", "in_pos_files", "duplicate_pin_numbers_are_jumpers")


def flatten(libname, symname):
    """Готовый блок для `lib_symbols` и карта выводов.

    Производные символы (`extends`) склеиваются с родителем: KiCad хранит в
    схеме уже развёрнутое определение, а не ссылку. Без этого BSS138 приехал бы
    без выводов вовсе — он унаследован от `Q_NMOS_GSD`.
    """
    block, tree = load_symbol(libname, symname)
    ext = find_all(tree, "extends")
    if ext:
        parent = ext[0][1][1]
        pblock, ptree = load_symbol(libname, parent)
        attrs = [c for c in top_children(pblock) if c[1:].split()[0] in ATTR_KEYS]
        subs = [c.replace(f'"{parent}_', f'"{symname}_')
                for c in top_children(pblock) if c.startswith('(symbol "')]
        props = [c for c in top_children(block) if c.startswith("(property ")]
        pins = symbol_pins(ptree)
        body = "\n".join(attrs + props + subs)
    else:
        children = [c for c in top_children(block)]
        pins = symbol_pins(tree)
        body = "\n".join(children)
    full = f'(symbol "{libname}:{symname}"\n{body}\n)'
    return full, pins


# --- Выдача UUID ----------------------------------------------------------
# Детерминированные: одинаковый запуск даёт одинаковый файл, и git показывает
# только осмысленные изменения, а не перетасовку идентификаторов.
_n = 0


def uid(tag=""):
    global _n
    _n += 1
    h = hashlib.sha1(f"{PROJECT}:{tag}:{_n}".encode()).hexdigest()
    return f"{h[0:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:32]}"


ROOT = uid("root")

# Артикулы склада JLCPCB. Все сверены по каталогу, а не восстановлены по
# памяти: неверный код — это не опечатка, а другая деталь на плате.
#
# Резисторы и конденсаторы из «базового» набора, за них не берут отдельную
# плату за настройку станка. BSS138 — расширенная позиция, и заменять его на
# базовый 2N7002 не стоит: у того порог отпирания вдвое выше, а весь смысл
# каскада в том, чтобы он открывался от 3.3 В.
#
# Светодиод именно красный. Зелёный в этом корпусе имеет прямое напряжение
# 3.3 В — ровно столько, сколько на шине, — и от неё попросту не зажёгся бы.
# У красного 1.8…2.4 В, и через 470 Ом получается около 3 мА: для вспышки
# длиной 100 мс это видно уверенно.
LCSC_10K = "C25804"       # 10 кОм 0603 1%
LCSC_470R = "C23179"      # 470 Ом 0603 1%
LCSC_100N = "C14663"      # 100 нФ 50 В 0603 X7R
LCSC_1U = "C15849"        # 1 мкФ 50 В 0603
LCSC_LED = "C2286"        # светодиод красный 0603, KT-0603R
LCSC_BSS138 = "C78284"    # BSS138 SOT-23, порог 0.8 В

# --- Что за плата ---------------------------------------------------------

# Каналы: цепь со стороны платы, цепь со стороны приёмника, контакт разъёма
# NRI, подпись. Порядок контактов не совпадает с порядком линий — это не
# опечатка, а распиновка из даташита, подтверждённая схемой предыдущего
# поколения устройства.
CHANNELS = [
    ("GP3", "LINE1", 7, "линия 1"),
    ("GP4", "LINE2", 8, "линия 2"),
    ("GP5", "LINE3", 9, "линия 3"),
    ("GP6", "LINE4", 10, "линия 4"),
    ("GP7", "LINE5", 3, "линия 5"),
    ("GP8", "LINE6", 4, "линия 6"),
    ("GP9", "INHIBIT", 6, "блокировка"),
]

# Контакт разъёма приёмника -> цепь. `return` не подключаем: он нужен только
# варианту Casino, где по нему идёт сигнал датчика приёма.
J1 = {1: "GND", 2: "+12V", 3: "LINE5", 4: "LINE6", 5: None,
      6: "INHIBIT", 7: "LINE1", 8: "LINE2", 9: "LINE3", 10: "LINE4"}

# Гребёнка к плате. 12 В идёт через неё же: отдельный клеммник означал бы
# второй кабель к устройству, которое и так стоит в тесном корпусе.
#
# Сигнальные выводы идут подряд и в том же порядке, что строки платы, —
# от этого веер разводки получается непересекающимся сам собой.
J2 = {1: "+12V", 2: "GND", 3: "+3V3", 4: "GND",
      5: "GP3", 6: "GP4", 7: "GP5", 8: "GP6", 9: "GP7", 10: "GP8", 11: "GP9",
      12: "GND"}


# --- Сборка файла ---------------------------------------------------------

SYMS = {
    "R": ("Device", "R"),
    "C": ("Device", "C"),
    "LED": ("Device", "LED"),
    "Q": ("Transistor_FET", "BSS138"),
    "J10": ("Connector_Generic", "Conn_02x05_Odd_Even"),
    "J1x12": ("Connector_Generic", "Conn_01x12"),
    "+3V3": ("power", "+3V3"),
    "+12V": ("power", "+12V"),
    "GND": ("power", "GND"),
    "FLAG": ("power", "PWR_FLAG"),
}

defs, pinmap = {}, {}
for key, (lib, name) in SYMS.items():
    block, pins = flatten(lib, name)
    defs[key] = block
    pinmap[key] = pins

body = []
nets = {}
bom = []


def emit(s):
    body.append(s)


def note(net):
    nets[net] = nets.get(net, 0) + 1


GRID = 1.27
off_grid = []


def check(*coords):
    """KiCad соединяет провода только на сетке 1.27 мм. Точка мимо неё выглядит
    соединённой, но цепи не образует — на схеме это невидимо, а на плате
    оборачивается непропаянной связью."""
    for c in coords:
        if abs(round(c / GRID) * GRID - c) > 1e-6:
            off_grid.append(c)


def wire(x1, y1, x2, y2):
    check(x1, y1, x2, y2)
    emit(f'\t(wire\n\t\t(pts (xy {x1} {y1}) (xy {x2} {y2}))\n'
         f'\t\t(stroke (width 0) (type default))\n\t\t(uuid "{uid("w")}")\n\t)')


def label(name, x, y, rot=0):
    check(x, y)
    note(name)
    emit(f'\t(label "{name}"\n\t\t(at {x} {y} {rot})\n\t\t(fields_autoplaced no)\n'
         f'\t\t(effects (font (size 1.27 1.27)) (justify left bottom))\n'
         f'\t\t(uuid "{uid("l")}")\n\t)')


def text(s, x, y, size=2.5):
    emit(f'\t(text "{s}"\n\t\t(at {x} {y} 0)\n'
         f'\t\t(effects (font (size {size} {size})) (justify left bottom))\n'
         f'\t\t(uuid "{uid("t")}")\n\t)')


def place(key, ref, value, x, y, footprint="", dnp=False, prop=(5.08, -1.27), lcsc=""):
    """Поставить компонент и вернуть координаты его выводов в схеме.

    `prop` — куда отнести позиционное обозначение и номинал. У разъёмов
    подписи иначе налезают на тело символа и на подписи цепей.
    """
    if not ref.startswith("#"):
        bom.append((ref, value, footprint, dnp))
    check(x, y)
    pins = pinmap[key]
    lib_id = f"{SYMS[key][0]}:{SYMS[key][1]}"
    plist = "\n".join(f'\t\t(pin "{n}" (uuid "{uid("p")}"))' for n in sorted(pins))
    hide = " (hide yes)" if ref.startswith("#") else ""
    emit(f'''\t(symbol
\t\t(lib_id "{lib_id}")
\t\t(at {x} {y} 0)
\t\t(unit 1)
\t\t(exclude_from_sim no)
\t\t(in_bom yes)
\t\t(on_board yes)
\t\t(dnp {"yes" if dnp else "no"})
\t\t(fields_autoplaced yes)
\t\t(uuid "{uid("s")}")
\t\t(property "Reference" "{ref}"
\t\t\t(at {x + prop[0]} {y + prop[1]} 0)
\t\t\t(effects (font (size 1.27 1.27)) (justify left){hide})
\t\t)
\t\t(property "Value" "{value}"
\t\t\t(at {x + prop[0]} {y + prop[1] + 2.54} 0)
\t\t\t(effects (font (size 1.27 1.27)) (justify left){hide})
\t\t)
\t\t(property "Footprint" "{footprint}"
\t\t\t(at {x} {y} 0)
\t\t\t(effects (font (size 1.27 1.27)) (hide yes))
\t\t)
\t\t(property "Datasheet" "~"
\t\t\t(at {x} {y} 0)
\t\t\t(effects (font (size 1.27 1.27)) (hide yes))
\t\t)
\t\t(property "LCSC" "{lcsc}"
\t\t\t(at {x} {y} 0)
\t\t\t(effects (font (size 1.27 1.27)) (hide yes))
\t\t)
{plist}
\t\t(instances
\t\t\t(project "{PROJECT}"
\t\t\t\t(path "/{ROOT}" (reference "{ref}") (unit 1))
\t\t\t)
\t\t)
\t)''')
    return {n: (x + px, y - py) for n, (px, py) in pins.items()}


def rail(kind, x, y):
    """Символ питания. Считаем его подключением к цепи — иначе проверка на
    оторванные цепи считала бы шины подозрительными."""
    note(kind)
    place(kind, f"#PWR{uid('n')[:5]}", kind, x, y)


def pwr_flag(kind, x, y):
    """Пометка «эту шину питает что-то снаружи».

    Нужна ERC: питание приходит с гребёнки, то есть с обычных пассивных
    выводов, и без флага проверка считает шины ничем не запитанными.
    """
    place("FLAG", f"#FLG{uid('f')[:5]}", "PWR_FLAG", x, y)
    wire(x, y, x, y + 2.54)
    rail(kind, x, y + 2.54)


# --- Разводка -------------------------------------------------------------
# Все координаты кратны 2.54 мм, чтобы выводы и концы проводов попадали на
# сетку соединений KiCad. Проверяется в `check()`.

U = 2.54

text("Переходник NRI G-13  <->  RP2350-Plus, 3.3 В", 25, 22, 3.5)
text("Двунаправленные BSS138: по одной и той же линии и читаем импульс монеты,", 25, 28, 1.8)
text("и блокируем канал, удерживая её в нуле.", 25, 32, 1.8)

# Разъём приёмника. Нумерация контактов и линий не совпадает — см. CHANNELS.
p = place("J10", "J1", "NRI G-13 (10 pin)", 24 * U, 20 * U,
          "Connector_IDC:IDC-Header_2x05_P2.54mm_Vertical", prop=(-6.35, -11.43))
for n, net in J1.items():
    x, y = p[str(n)]
    ex = x - 6.35 if n % 2 else x + 6.35
    if net is None:
        # `return` не подключаем: он нужен только варианту Casino, где по нему
        # идёт сигнал датчика приёма.
        emit(f'\t(no_connect (at {x} {y}) (uuid "{uid("nc")}"))')
        continue
    wire(x, y, ex, y)
    label(net, ex, y)

# Гребёнка к плате: и 3.3 В, и 12 В, и сигналы — одним кабелем.
p = place("J1x12", "J2", "RP2350-Plus + 12V", 24 * U, 60 * U,
          "Connector_PinHeader_2.54mm:PinHeader_1x12_P2.54mm_Vertical", prop=(-3.81, -19.05))
for n, net in J2.items():
    x, y = p[str(n)]
    wire(x, y, x - 6.35, y)
    label(net, x - 6.35, y)

# Питание приходит с гребёнки, то есть с обычных пассивных выводов. Без этих
# флагов ERC считает шины ничем не запитанными.
text("Питание — всё с гребёнки:", 25, 100, 1.8)
text("3.3 В от платы, 12 В от внешнего источника", 25, 104, 1.8)
for i, kind in enumerate(("+3V3", "+12V", "GND")):
    pwr_flag(kind, (14 + 5 * i) * U, 43 * U)

# Развязка по питанию.
# 1 мкФ, а не 10: в корпусе 0603 десять микрофарад бывают только на 10 В, а
# эта банка сидит на шине 12 В. Ток тут копеечный, ёмкости хватает с запасом.
for i, (kind, val, ref, code) in enumerate((("+3V3", "100n", "C1", LCSC_100N),
                                            ("+12V", "100n", "C2", LCSC_100N),
                                            ("+12V", "1u", "C3", LCSC_1U))):
    x = (38 + 5 * i) * U
    p = place("C", ref, val, x, 40 * U, "Capacitor_SMD:C_0603_1608Metric", lcsc=code)
    wire(*p["1"], x, 37 * U)
    rail(kind, x, 37 * U)
    wire(*p["2"], x, 43 * U)
    rail("GND", x, 43 * U)

# Семь каналов. Отличаются только именами цепей, поэтому и генерируются
# циклом: скопировать семь раз мышью — верный способ ошибиться в одном.
for i, (gp, line, _pin, caption) in enumerate(CHANNELS):
    x = (52 + 13 * i) * U
    text(caption, x - 8, 34 * U, 1.8)

    # Преобразователь уровней. Затвор на 3.3 В, исток к плате, сток к
    # приёмнику — так BSS138 работает в обе стороны.
    q = place("Q", f"Q{i+1}", "BSS138", x, 44 * U, "Package_TO_SOT_SMD:SOT-23",
              lcsc=LCSC_BSS138)
    gx, gy = q["1"]
    wire(gx, gy, gx - 3 * U, gy)
    rail("+3V3", gx - 3 * U, gy)

    dx, dy = q["3"]                       # сток — сторона приёмника
    wire(dx, dy, dx, 36 * U)
    label(line, dx, 36 * U)

    sx, sy = q["2"]                       # исток — сторона платы
    wire(sx, sy, sx, 51 * U)
    label(gp, sx, 51 * U)

    # Подтяжка со стороны платы. Встроенной в RP2350 хватило бы, но 10 кОм
    # надёжнее на проводах внутри корпуса автомата. Для GP9 этот же резистор
    # держит вход блокировки закрытым, пока контроллер в сбросе.
    r = place("R", f"R{i+1}", "10k", sx, 57 * U, "Resistor_SMD:R_0603_1608Metric",
              lcsc=LCSC_10K)
    wire(*r["1"], sx, 54 * U)
    rail("+3V3", sx, 54 * U)
    wire(*r["2"], sx, 60 * U)
    label(gp, sx, 60 * U)

    # Подтяжка со стороны приёмника. Для линий монет она нужна, только если
    # приёмник не тянет их сам — измерьте перед установкой. Для входа
    # блокировки обязательна: без неё уровень блокировки не наберётся.
    inhibit = line == "INHIBIT"
    r = place("R", f"R{i+11}", "10k", dx, 28 * U,
              "Resistor_SMD:R_0603_1608Metric", dnp=not inhibit, lcsc=LCSC_10K)
    wire(*r["1"], dx, 25 * U)
    rail("+12V", dx, 25 * U)
    wire(*r["2"], dx, 31 * U)
    label(line, dx, 31 * U)

    # Индикатор. На GP9 его нет намеренно: светодиод с резистором перетянул бы
    # подтяжку 10 кОм и сломал бы то самое «в сбросе заблокировано», ради чего
    # она там стоит.
    if inhibit:
        continue
    d = place("LED", f"D{i+1}", "LED", sx, 71 * U, "LED_SMD:LED_0603_1608Metric",
              lcsc=LCSC_LED)
    kx, ky = d["1"]
    wire(kx, ky, kx - 3 * U, ky)
    label(gp, kx - 3 * U, ky)
    ax, ay = d["2"]
    wire(ax, ay, ax + 3 * U, ay)
    r = place("R", f"R{i+21}", "470", ax + 3 * U, 67 * U, "Resistor_SMD:R_0603_1608Metric",
              lcsc=LCSC_470R)
    wire(ax + 3 * U, ay, *r["2"])
    wire(*r["1"], ax + 3 * U, 64 * U)
    rail("+3V3", ax + 3 * U, 64 * U)

text("Светодиод горит, пока линия притянута к нулю — то есть идёт импульс монеты.", 130, 200, 1.8)
text("R11…R16 ставить, только если приёмник не подтягивает линии сам: сначала измерьте.", 130, 204, 1.8)
text("R17 обязателен — без него на входе полной блокировки не наберётся 3,5 В.", 130, 208, 1.8)

# --- Запись ---------------------------------------------------------------

libs = "\n".join(defs[k] for k in SYMS)
sch = f'''(kicad_sch
\t(version 20250114)
\t(generator "gen_sch.py")
\t(generator_version "9.0")
\t(uuid "{ROOT}")
\t(paper "A3")
\t(title_block
\t\t(title "NRI G-13 <-> RP2350-Plus level shifter")
\t\t(rev "A")
\t\t(comment 1 "Двунаправленные BSS138 на шесть линий монет и линию полной блокировки")
\t)
\t(lib_symbols
{libs}
\t)
{chr(10).join(body)}
\t(sheet_instances
\t\t(path "/" (page "1"))
\t)
)
'''

with open(f"{PROJECT}.kicad_sch", "w") as f:
    f.write(sch)

# --- Проверки -------------------------------------------------------------
# Дешевле поймать оторванную цепь здесь, чем на собранной плате.

problems = []
for net, count in sorted(nets.items()):
    if count < 2:
        problems.append(f"цепь {net} встречается один раз — оторвана")

expected = {c[0] for c in CHANNELS} | {c[1] for c in CHANNELS} | {"+3V3", "+12V", "GND"}
missing = expected - set(nets)
if missing:
    problems.append(f"нет цепей: {sorted(missing)}")

if off_grid:
    problems.append(f"{len(off_grid)} координат мимо сетки {GRID} мм: "
                    f"{sorted(set(off_grid))[:5]}")

depth = 0
for ch in sch:
    depth += (ch == "(") - (ch == ")")
if depth:
    problems.append(f"скобки не сходятся: {depth}")

print(f"компонентов: {len(bom)}, цепей: {len(nets)}, скобки {'сходятся' if not depth else 'НЕТ'}")
for net, count in sorted(nets.items()):
    print(f"  {net:10} {count} подключений")
if problems:
    print("\nПРОБЛЕМЫ:")
    for p in problems:
        print("  -", p)
    raise SystemExit(1)

with open("bom.md", "w") as f:
    f.write("# Спецификация\n\n| Позиция | Номинал | Корпус | Ставить |\n|---|---|---|---|\n")
    for ref, val, fp, dnp in sorted(bom, key=lambda b: (b[0][0], int(re.sub(r"\D", "", b[0]) or 0))):
        f.write(f"| {ref} | {val} | {fp.split(':')[-1] if fp else '—'} | "
                f"{'нет (по месту)' if dnp else 'да'} |\n")
print("\nзаписано:", f"{PROJECT}.kicad_sch", "и bom.md")

"""Чтение посадочных мест из библиотек KiCad."""
import os
from gen_sch import tokenize, parse, head, find_all

FPDIR = "/Applications/KiCad/KiCad.app/Contents/SharedSupport/footprints"


def load_fp(lib, name):
    path = os.path.join(FPDIR, f"{lib}.pretty", f"{name}.kicad_mod")
    text = open(path).read()
    tree, _ = parse(tokenize(text))
    return text, tree


def fp_pads(tree):
    """Площадки: номер -> (x, y, слои, форма, размер)."""
    out = {}
    for pad in find_all(tree, "pad"):
        num = pad[1][1]
        at = find_all(pad, "at")[0]
        size = find_all(pad, "size")[0]
        out[num] = (float(at[1][1]), float(at[2][1]),
                    float(size[1][1]), float(size[2][1]))
    return out

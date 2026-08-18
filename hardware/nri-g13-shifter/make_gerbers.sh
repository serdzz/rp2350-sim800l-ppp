#!/bin/sh
# Производственный комплект: гербера, сверловка, архив для загрузки на завод.
#
# Заливку заполняет KiCad, а не gen_pcb.py, поэтому перед выводом обязателен
# --check-zones. Без него на плату уехали бы пустые зоны, то есть плата без
# земли.
set -e
cd "$(dirname "$0")"
BOARD=nri-g13-shifter
OUT=gerber

rm -rf "$OUT" && mkdir -p "$OUT"

kicad-cli pcb export gerbers \
    --check-zones --subtract-soldermask --no-protel-ext \
    -l F.Cu,B.Cu,F.SilkS,B.SilkS,F.Mask,B.Mask,F.Paste,B.Paste,Edge.Cuts \
    -o "$OUT/" "$BOARD.kicad_pcb"

kicad-cli pcb export drill \
    --format excellon --drill-origin absolute --excellon-units mm \
    --excellon-separate-th --generate-map --map-format pdf \
    --generate-report --report-path "$OUT/drill-report.txt" \
    -o "$OUT/" "$BOARD.kicad_pcb"

# Комплект для монтажа: перечень элементов и координаты установки.
# --exclude-dnp обязателен: R11..R16 ставить не нужно, и завод не должен
# узнать об этом из примечания в письме.
mkdir -p assembly
kicad-cli sch export bom \
    --fields 'Value,Reference,Footprint,LCSC,${QUANTITY}' \
    --labels 'Comment,Designator,Footprint,LCSC Part #,Qty' \
    --group-by 'Value,Footprint' --exclude-dnp \
    -o assembly/bom.csv "$BOARD.kicad_sch"

# --exclude-fp-th: выводные разъёмы на автоматический монтаж не идут, и
# оставлять их в файле — значит получить вопрос от завода вместо платы.
# Двадцать два вывода паяются руками за десять минут.
kicad-cli pcb export pos --format csv --units mm --side both \
    --exclude-dnp --exclude-fp-th \
    -o assembly/cpl.csv "$BOARD.kicad_pcb"

# Перечень приводим ровно к тому, что ставит станок: строка в перечне без
# координат установки — это лишний вопрос от завода.
python3 - <<'PYEOF'
import csv
placed = {r["Ref"] for r in csv.DictReader(open("assembly/cpl.csv"))}
rows = list(csv.DictReader(open("assembly/bom.csv")))
keep = []
for r in rows:
    refs = [x for x in r["Designator"].replace(" ", "").split(",")]
    expanded = []
    for x in refs:
        if "-" in x:                      # диапазон вида D1-D6
            a, b = x.split("-")
            pre = "".join(c for c in a if c.isalpha())
            expanded += [f"{pre}{i}" for i in range(int(a[len(pre):]), int(b[len(pre):]) + 1)]
        else:
            expanded.append(x)
    if any(e in placed for e in expanded):
        keep.append(r)
with open("assembly/bom.csv", "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=rows[0].keys(), quoting=csv.QUOTE_ALL)
    w.writeheader()
    w.writerows(keep)
PYEOF

rm -f "$BOARD-gerber.zip"
(cd "$OUT" && zip -q "../$BOARD-gerber.zip" *.gbr *.gbrjob *.drl)

echo
echo "Готово. На завод — $BOARD-gerber.zip"
grep -E "Total|T[0-9]" "$OUT/drill-report.txt"

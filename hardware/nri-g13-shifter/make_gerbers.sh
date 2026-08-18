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
    --group-by 'Value,Footprint' --exclude-dnp --ref-range-delimiter '' \
    -o assembly/bom.csv "$BOARD.kicad_sch"

# --exclude-fp-th: выводные разъёмы на автоматический монтаж не идут, и
# оставлять их в файле — значит получить вопрос от завода вместо платы.
# Двадцать два вывода паяются руками за десять минут.
kicad-cli pcb export pos --format csv --units mm --side both \
    --exclude-dnp --exclude-fp-th \
    -o assembly/cpl.csv "$BOARD.kicad_pcb"

# KiCad и JLCPCB называют одни и те же колонки по-разному, и завод на своём
# файле спотыкается молча: «File processing failed» без подробностей.
python3 - <<'PYEOF'
import csv
rows = list(csv.DictReader(open("assembly/cpl.csv")))
with open("assembly/cpl.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["Designator", "Mid X", "Mid Y", "Rotation", "Layer"])
    for r in rows:
        w.writerow([r["Ref"], f'{float(r["PosX"]):.4f}', f'{float(r["PosY"]):.4f}',
                    f'{float(r["Rot"]):.2f}', r["Side"].capitalize()])
PYEOF

# Перечень приводим ровно к тому, что ставит станок: строка в перечне без
# координат установки — это лишний вопрос от завода.
python3 - <<'PYEOF'
import csv
placed = {r["Designator"] for r in csv.DictReader(open("assembly/cpl.csv"))}
rows = list(csv.DictReader(open("assembly/bom.csv")))
keep = []
for r in rows:
    if any(x in placed for x in r["Designator"].replace(" ", "").split(",")):
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

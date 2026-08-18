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

kicad-cli pcb export pos --format csv --units mm --side both --exclude-dnp \
    -o assembly/cpl.csv "$BOARD.kicad_pcb"

rm -f "$BOARD-gerber.zip"
(cd "$OUT" && zip -q "../$BOARD-gerber.zip" *.gbr *.gbrjob *.drl)

echo
echo "Готово. На завод — $BOARD-gerber.zip"
grep -E "Total|T[0-9]" "$OUT/drill-report.txt"

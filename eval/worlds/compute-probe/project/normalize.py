import csv
from collections import defaultdict
from decimal import Decimal

factors = {"L": 1000, "mL": 1}
totals = defaultdict(int)
seen = set()
with open("samples.csv", newline="", encoding="utf-8") as source:
    for row in csv.DictReader(source):
        if row["sample"] in seen or not row["volume"] or row["unit"] not in factors:
            continue
        seen.add(row["sample"])
        amount = Decimal(row["volume"]) * factors[row["unit"]]
        if amount != amount.to_integral_value():
            raise ValueError("volume is not a whole number of millilitres")
        totals[row["plot"]] += int(amount)

with open("totals.csv", "w", newline="", encoding="utf-8") as output:
    writer = csv.writer(output)
    writer.writerow(["plot", "millilitres"])
    writer.writerows(sorted(totals.items()))

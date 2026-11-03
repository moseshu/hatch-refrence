def total(lines):
    # TODO: handle currency rounding
    return sum(l["amount"] for l in lines)

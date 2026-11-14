"""Summary statistics over exported balances."""

import json
import sys


def load(path):
    with open(path) as handle:
        return json.load(handle)


def mean(values):
    if not values:
        return 0
    return sum(values) / len(values)


def main():
    balances = load(sys.argv[1])
    cents = [row["cents"] for row in balances]
    print(f"accounts: {len(cents)}")
    print(f"mean: {mean(cents) / 100:.2f}")
    print(f"max: {max(cents, default=0) / 100:.2f}")


if __name__ == "__main__":
    main()

"""Other half of the case d (D4) cross-file cycle. See even.py."""
from even import is_even


def is_odd(n):
    if n == 0:
        return False
    return is_even(n - 1)

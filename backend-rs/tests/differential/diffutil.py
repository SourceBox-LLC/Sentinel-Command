"""Where two response bodies differ, rather than two truncated dumps.

Both HTTP differentials print this. It exists because a difference deep
inside a large body — an archive member, one camera in a list of
twenty-one — printed as two dumps that agreed for their first couple of
hundred characters and said nothing at all. Twice.
"""

import json


def value_diff(python, rust, path="body", out=None, limit=12):
    """The paths at which two response bodies differ.

    Walks dicts and lists in step and reports leaves, so a difference
    deep inside an archive member reads as
    `body.<zip>.files.cameras.json[3].last_seen` rather than as two
    dumps that agree for their first 260 characters.
    """
    out = [] if out is None else out
    if len(out) >= limit:
        return out
    if isinstance(python, dict) and isinstance(rust, dict):
        for key in sorted(set(python) | set(rust)):
            if key not in python:
                out.append(f"{path}.{key}: only in rust = {short(rust[key])}")
            elif key not in rust:
                out.append(f"{path}.{key}: only in python = {short(python[key])}")
            elif python[key] != rust[key]:
                value_diff(python[key], rust[key], f"{path}.{key}", out, limit)
            if len(out) >= limit:
                break
        return out
    if isinstance(python, list) and isinstance(rust, list):
        if len(python) != len(rust):
            out.append(f"{path}: {len(python)} items in python, {len(rust)} in rust")
        for i, (a, b) in enumerate(zip(python, rust)):
            if a != b:
                value_diff(a, b, f"{path}[{i}]", out, limit)
            if len(out) >= limit:
                break
        return out
    out.append(f"{path}: python={short(python)} rust={short(rust)}")
    return out


def short(value, width=90):
    text = json.dumps(value, sort_keys=True) if not isinstance(value, str) else repr(value)
    return text if len(text) <= width else text[:width] + "..."



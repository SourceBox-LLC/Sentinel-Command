"""Every ported route must be gated exactly as Python gates it.

Porting a route means re-declaring its auth. Nothing checks that the
re-declaration matches — a route Python guards with `require_admin` can
be ported behind `require_view` and every differential still passes,
because the differentials ran as an admin until very recently and an
admin satisfies both.

That is the same class of hole as the rate limits: the decorator lives
on the Python handler and simply does not come along. This compares the
two tables directly.

Mapping:

    Depends(require_view)           -> RequireView
    Depends(require_admin)          -> RequireAdmin
    Depends(require_active_billing) -> RequireActiveBilling
    Depends(get_current_user)       -> AuthUser
    (no auth dependency)            -> no extractor

Usage: auth_parity.py     (exits non-zero on a mismatch)
"""
import ast
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
BACKEND_RS = HERE.parent.parent
BACKEND = BACKEND_RS.parent / "backend"

# python dependency -> the Rust extractor that means the same thing
EQUIVALENT = {
    "require_view": "RequireView",
    "require_admin": "RequireAdmin",
    "require_active_billing": "RequireActiveBilling",
    "get_current_user": "AuthUser",
}


def python_gates():
    """(METHOD, path) -> the auth dependency name, or None."""
    out = {}
    for f in sorted((BACKEND / "app/api").glob("*.py")):
        src = f.read_text()
        m = re.search(r'APIRouter\((?:prefix="([^"]*)")?', src)
        prefix = m.group(1) if m and m.group(1) else ""
        for node in ast.walk(ast.parse(src)):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            path = method = None
            for d in node.decorator_list:
                seg = ast.get_source_segment(src, d) or ""
                rm = re.match(r'router\.(get|post|put|patch|delete)\(\s*"([^"]*)"', seg)
                if rm:
                    method, path = rm.group(1).upper(), rm.group(2)
            if path is None:
                continue
            sig = ast.get_source_segment(src, node) or ""
            sig = sig.split("):", 1)[0]
            gate = next((g for g in EQUIVALENT if f"Depends({g})" in sig), None)
            out[(method, prefix + path)] = gate
    return out


def rust_gates():
    """handler fn name -> the extractor it takes, or None."""
    out = {}
    for f in sorted((BACKEND_RS / "src/api").glob("*.rs")):
        src = f.read_text()
        for m in re.finditer(r"pub async fn (\w+)\(", src):
            name = m.group(1)
            i = m.end() - 1
            depth, j = 0, i
            while j < len(src):
                if src[j] == "(":
                    depth += 1
                elif src[j] == ")":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            args = src[i:j]
            found = None
            for extractor in ("RequireActiveBilling", "RequireAdmin", "RequireView"):
                if re.search(rf"\b{extractor}\b", args):
                    found = extractor
                    break
            if found is None and re.search(r":\s*AuthUser\b", args):
                found = "AuthUser"
            out[name] = found
    return out


def rust_routes():
    src = (BACKEND_RS / "src/app.rs").read_text()
    routes = []
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', src):
        i = src.rindex("(", 0, m.end())
        depth, j = 0, i
        while j < len(src):
            if src[j] == "(":
                depth += 1
            elif src[j] == ")":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        body = src[m.end():j]
        if "still_python" in body:
            continue
        path = m.group(1)
        ported = re.search(r"ported\(\s*(?:api::)?([\w:]+)", body)
        if ported:
            routes.append(("GET", path, ported.group(1).split("::")[-1]))
            continue
        for verb in ("get", "post", "put", "patch", "delete"):
            for h in re.finditer(
                rf"(?:^|[^\w])(?:axum::routing::)?{verb}\(\s*(?:api::)?([\w:]+)", body
            ):
                routes.append((verb.upper(), path, h.group(1).split("::")[-1]))
    return routes


def main():
    py = python_gates()
    handlers = rust_gates()
    routes = sorted(set(rust_routes()))

    bad = 0
    print(f"checking {len(routes)} ported method+path pairs\n")
    for method, path, handler in routes:
        if handler == "health":
            continue
        if (method, path) not in py:
            print(f"  FAIL  {method:<6} {path:<46} no matching python route")
            bad += 1
            continue
        want = py[(method, path)]
        got = handlers.get(handler)
        want_rust = EQUIVALENT.get(want) if want else None
        if want_rust != got:
            print(f"  FAIL  {method:<6} {path:<46} python={want or 'open'} "
                  f"expects {want_rust or 'no extractor'}, rust has {got or 'none'}")
            bad += 1
        else:
            print(f"  ok    {method:<6} {path:<46} {want or 'open'}")

    print(f"\n{bad} mismatch(es)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())

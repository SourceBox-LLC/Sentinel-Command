#!/usr/bin/env python3
"""Prove a harness has teeth: inject each bug in a spec, run, restore.

Every checker in this directory has been wrong in the false-pass
direction at least once, so a new case list is not trusted until a
deliberately broken port makes it fail. This driver used to be
rewritten per slice in a scratch directory, and each rewrite relearned
the same two lessons the hard way:

* restore in `finally`, never after the run — one version died on a
  timeout mid-mutation and left `if false` where the scoped-key org
  filter belongs: a tenant leak sitting in the source;
* restart tiers only through `tiers.sh` — one version started Rust from
  its own inlined environment, forgot LOCAL_ADMIN_PASSWORD_HASH, and
  added a constant two-case divergence to every result it scored.

Spec format (tests/differential/mutations/*.json):

    {
      "restart_rust": true,                 # rebuild + restart :8000 per mutation
      "harness": [["tests/differential/http_run.sh"], ...],
      "env": {"DIFF_ONLY": "sentinel"},      # optional, passed to harness
      "mutations": [
        {"name": "...", "file": "src/...", "old": "...", "new": "...",
         "equivalent": "optional: why no observable behaviour changes"}
      ]
    }

A mutation marked `equivalent` is expected to be caught by nothing, and
the reason is printed — so an equivalent mutant is a documented
decision rather than a quiet MISS someone later "fixes" by adding a
case that cannot fail.

Exit status is 1 if any non-equivalent mutation was missed.

Three safety nets, added after a stopped run left "a revoked agent key
still authenticates" sitting in src/api/sentinel.rs — the SIGINT went
to the bash wrapper, the Python process was orphaned mid-mutation, and
its `finally` only ran when it was found and interrupted directly:

* it refuses to start unless every file a spec mutates is unmodified
  in git, so `git checkout -- <file>` is always a correct recovery;
* SIGTERM and SIGHUP are converted into the same KeyboardInterrupt path
  as SIGINT, so any ordinary kill still restores the source;
* while a mutation is applied, target/mutation-in-progress.json names
  it, and a later run refuses to start while that marker exists.
"""
from __future__ import annotations

import json
import os
import signal
import pathlib
import re
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
RS = HERE.parents[1]

COUNT = re.compile(r"(\d+)/(\d+) identical[^\n]*?(\d+) differing")
GUARDS = ("COVERAGE TOO THIN", "REFUSING", "FIXTURE TOO THIN", "WATCHED TABLES WITH NO ROWS")


TIER = {"action": "restart-rust", "port": 8000}


def restart_rust() -> bool:
    """Rebuild and restart the Rust tier the spec targets.

    `"tier": "clerk"` in a spec means the Clerk-mode pair on 8100/8101,
    for behaviour that exists only under Clerk.
    """
    r = subprocess.run([str(HERE / "tiers.sh"), TIER["action"]],
                       capture_output=True, text=True, cwd=RS, timeout=900)
    return f":{TIER['port']} healthy" in r.stdout


def run_harness(cmds, env) -> tuple[int | None, str]:
    total, notes = 0, []
    for cmd in cmds:
        r = subprocess.run([str(RS / cmd[0]), *cmd[1:]], capture_output=True, text=True,
                           cwd=RS, timeout=3600, env={**os.environ, **env})
        out = r.stdout + r.stderr
        m = COUNT.search(out)
        if m:
            total += int(m.group(3))
            continue
        guard = next((g for g in GUARDS if g in out), None)
        if guard:
            notes.append(f"{pathlib.Path(cmd[0]).name}: guard '{guard}'")
            total += 1  # a guard firing on a mutant is a catch
            continue
        if "failed" in out.lower() or r.returncode not in (0, 1):
            notes.append(f"{pathlib.Path(cmd[0]).name}: no result (rc={r.returncode})")
            return None, "; ".join(notes)
    return total, "; ".join(notes)


MARKER = RS / "target" / "mutation-in-progress.json"


def _interrupt(signum, _frame):
    raise KeyboardInterrupt(f"signal {signum}")


def main() -> int:
    """Run, and always leave the tier built from the restored source.

    The rebuild used to follow the loop, so an interrupted run restored
    the file but left :8000 running the last mutant's binary — the next
    differential then reported that mutant's diffs as if they were real.

    On the normal path the rebuild now happens before the summary is
    printed, so that line means the tier is back up on restored source
    and a script waiting for it is not racing a restart. This stays for
    the interrupted path, where nothing else will do it.
    """
    rebuild = False
    try:
        code, rebuild = _main()
        return code
    except KeyboardInterrupt:
        rebuild = True
        print("\ninterrupted — source restored, rebuilding the Rust tier", flush=True)
        return 130
    finally:
        if rebuild:
            restart_rust()


def _main() -> tuple[int, bool]:
    if len(sys.argv) < 2:
        print(__doc__)
        return 64, False
    args = sys.argv[1:]
    start_from = None
    if "--from" in args:
        i = args.index("--from")
        start_from = args[i + 1]
        del args[i:i + 2]
    spec = json.loads(pathlib.Path(args[0]).read_text())
    if spec.get("tier") == "clerk":
        TIER.update(action="restart-rust-clerk", port=8100)

    if MARKER.exists():
        info = json.loads(MARKER.read_text())
        print(f"REFUSING: a previous run was killed with a mutation applied:\n"
              f"  {info['name']}\n  in {info['file']}\n"
              f"Restore it with `git checkout -- {info['file']}`, rebuild, "
              f"then delete {MARKER}.")
        return 2, False
    files = sorted({m["file"] for m in spec["mutations"]})
    dirty = subprocess.run(["git", "diff", "--name-only", "--", *files],
                           capture_output=True, text=True, cwd=RS).stdout.split()
    if dirty:
        print("REFUSING: these files have uncommitted changes, so git could not "
              "restore them if this run were killed mid-mutation:\n  " + "\n  ".join(dirty))
        return 2, False

    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, _interrupt)
    only = args[1] if len(args) > 1 else None
    env = spec.get("env", {})
    results = []

    started = start_from is None
    for mut in spec["mutations"]:
        # --from NAME resumes a stopped run at the first mutation whose
        # name contains NAME, so a hang late in a run does not cost the
        # whole run again.
        if not started:
            if start_from not in mut["name"]:
                continue
            started = True
        if only and only not in mut["name"]:
            continue
        path = RS / mut["file"]
        original = path.read_text()
        if mut["old"] not in original:
            results.append((mut, "PATCH DID NOT APPLY", ""))
            print(f"  !!  patch did not apply: {mut['name']}", flush=True)
            continue
        try:
            MARKER.parent.mkdir(parents=True, exist_ok=True)
            MARKER.write_text(json.dumps({"name": mut["name"], "file": mut["file"]}))
            path.write_text(original.replace(mut["old"], mut["new"], 1))
            if spec.get("restart_rust") and not restart_rust():
                results.append((mut, "DID NOT BUILD", ""))
                print(f"  !!  did not build: {mut['name']}", flush=True)
                continue
            caught, notes = run_harness(spec["harness"], env)
            results.append((mut, caught, notes))
            print(f"  {str(caught):>5}  {mut['name']}" + (f"   [{notes}]" if notes else ""), flush=True)
        finally:
            path.write_text(original)
            MARKER.unlink(missing_ok=True)

    # Leave the tier built from the restored source *before* the
    # summary, not after: the summary is what a watching script waits
    # for, and printing it while :8000 is still being restarted means
    # the next differential races the restart. One read run reported
    # eleven differences that way and the next four were green.
    if spec.get("restart_rust"):
        restart_rust()

    missed = 0
    print("\n=== summary ===")
    for mut, caught, notes in results:
        eq = mut.get("equivalent")
        if eq:
            flag = "EQUIV" if caught == 0 else "EQ?? "
            print(f"{flag} {str(caught):>5}  {mut['name']}\n              ({eq})")
            continue
        ok = isinstance(caught, int) and caught > 0
        missed += 0 if ok else 1
        print(f"{'OK   ' if ok else 'MISS '} {str(caught):>5}  {mut['name']}")
    # Already rebuilt above, so `main`'s finally has nothing left to do
    # on the normal path — it stays for the interrupted one.
    return (1 if missed else 0), False


if __name__ == "__main__":
    sys.exit(main())

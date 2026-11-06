#!/usr/bin/env python3
"""Run `hatch eval --live` against a live-model gateway without ever
seeing the real key.

The registered credential (named by --credential) is fetched from authd
as an ``hsurr:`` surrogate (in memory only, never printed, never written
to a file by this script). The surrogate is passed to the hatch binary
only through the child process environment as HATCH_LLM_API_KEY.
Sentinel swaps the surrogate for the real key on egress to the
configured gateway host only.

Usage:
  python3 scripts/run_live_eval.py --model <model> --base-url <url>
      [--credential <name>] [--world eval/worlds/zh-office]
      [--repeat 3] [--max-calls 120] [--out /tmp/live-eval/zh-office]

The model, provider and base URL have no defaults: pass them explicitly
every time. Any extra arguments after `--` are passed through to
`hatch eval`.
"""
import argparse
import os
import subprocess
import sys

sys.path.insert(0, "/opt/muse/skills/custom-connector-setup/bin")
from dynamic_credentials import dynamic_credential_entry

HATCH_BIN = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "target", "debug", "hatch",
)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--provider", default="openai_responses")
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--credential", required=True,
                        help="authd credential name to fetch")
    parser.add_argument("--world", default=None)
    parser.add_argument("--repeat", default=None)
    parser.add_argument("--max-calls", default=None)
    parser.add_argument("--out", default=None)
    args, rest = parser.parse_known_args()
    passthrough = rest[1:] if rest and rest[0] == "--" else rest

    entry = dynamic_credential_entry(args.credential)
    surrogate = str(entry["surrogate"]).strip()
    if not surrogate.startswith("hsurr:"):
        raise SystemExit("authd did not return a surrogate")

    env = dict(os.environ)
    env["HATCH_LLM_API_KEY"] = surrogate
    env["HATCH_LLM_PROVIDER"] = args.provider
    env["HATCH_LLM_BASE_URL"] = args.base_url
    env["HATCH_LLM_MODEL"] = args.model

    cmd = [HATCH_BIN, "eval", "--live"]
    if args.world:
        cmd += ["--world", args.world]
    if args.repeat:
        cmd += ["--repeat", args.repeat]
    if args.max_calls:
        cmd += ["--max-calls", args.max_calls]
    if args.out:
        cmd += ["--out", args.out]
    cmd += passthrough
    result = subprocess.run(cmd, env=env)
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
